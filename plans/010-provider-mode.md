# Plan 010: Provider Mode — catalog setup as a chat mode

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat d637cd6..HEAD -- crates/engine/src/agent.rs crates/engine/src/rpc.rs crates/engine/src/tools/model_setup.rs crates/engine/src/provider_settings.rs crates/engine/src/plan_mode.rs crates/proto/src/entities.rs crates/rpc/src/lib.rs crates/doc/src crates/ui/src/settings/providers crates/ui/src/transcript crates/ui/src/pickers crates/ui/src/composer crates/engine/tests/model_setup_rpc.rs`.
> Line numbers below were read at `d637cd6` (ADR-0036 landed); offsets of a
> few lines in `agent.rs` are expected. Compare the "Current state" excerpts
> against the live code and treat a structural mismatch as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED — touches the Turn toolset/prompt assembly, adds two doc part kinds (schema), and changes the catalog write's staleness gate. The ADR-0029 invariant (only a human writes, exactly as stored) and the ADR-0031 key rules must survive unchanged.
- **Depends on**: none (ADR-0036, `d637cd6`)
- **Category**: feature
- **Planned at**: commit `d637cd6`, 2026-09-24

## Why this matters

ADR-0037 (`docs/adr/0037-provider-mode-is-a-chat-mode.md`) replaces the
Settings-dialog setup chat (ADR-0030) with **Provider Mode**: a chat-level
mode, like Plan Mode (ADR-0025), in which an ordinary chat adds and updates
providers. The dialog had structural problems — a chat inside a modal, a
history deleted on close, a separate review panel, and a key flow that needed
a proposal before a key could be requested (add provider → Settings → key →
back to chat → probe). Read ADR-0037 before starting; every decision below is
from it and is not up for re-design.

## Current state

Engine:

- `crates/engine/src/tools/model_setup.rs` (2144 lines) holds the whole
  feature: `CatalogChange` (line 68, 5 actions, **not** serde), `parse_change`
  (112), `StoredProposal` (186), `store_proposal` (200), `build_proposal`
  (387), `apply_changes` (588), the probe section (~743–990),
  `setup_system_prompt` (999), `change_view` (1033), `apply_stored` (1113),
  `run_proposal_tool` (1170), `create_model_proposal_tool` (~1444),
  `PendingKeyRequest` / `key_request_target` (1499) /
  `planned_probe_key_allowed` / `create_request_provider_key_tool` (1622).
- `crates/engine/src/agent.rs`: `ChatRuntime` (114) holds `proposals`,
  `key_request`, `approved_key_destinations` in memory (~163–180);
  `AgentRuntime::chat` (802) lazily `ChatRuntime::load`s; `remove_chat` (839)
  deletes a chat's files; `AgentRun.plan_mode` / `setup_scope` (1810/1815);
  `if setup_scope { system_prompt = setup_system_prompt(..) }` (1912) —
  **replaces** the workspace prompt; the `AgentEvent::ToolExecutionEnd`
  handler (2092) updates `base_parts` and calls `resolve_tool_part` (1183)
  with `result.details` in hand; toolset `else if setup_scope` (2385),
  MCP mount guard `!setup_scope && !plan_mode` (2431), Plan Mode append +
  `read_only_tool_allowed` filter (2448–2451).
- `crates/engine/src/rpc.rs`: `start_model_setup_chat` (250–315) creates a
  hidden archived `ChatScope::ModelSetup` chat and deletes the older one;
  admission snapshots `planning` and `setup_scope` (878–1017);
  `enter_plan_mode` / `exit_plan_mode` (1403/1436) settle plan cards via
  `plan_mode::settle_plan_cards` (1457/1532); dispatch for
  `APPLY_MODEL_PROPOSAL`, `LIST_MODEL_PROPOSALS`, `DISCARD_MODEL_PROPOSAL`,
  `START_MODEL_SETUP_CHAT`, `GET_PROVIDER_KEY_REQUEST`,
  `SETTLE_PROVIDER_KEY_REQUEST` (2525–2620); Plan Mode dispatch (3605–3607).
- `crates/engine/src/plan_mode.rs` (337 lines) is the template:
  `planning_system_block`, `read_only_tool_allowed`, `settle_plan_cards`
  (166 — mutate parts in `chat.transcript`, then `chat.persist_entry` each
  changed entry, ADR-0032).
- `crates/engine/src/provider_settings.rs:165` `replace_if_unchanged`
  compares the **whole** snapshot — in a long-lived chat, writing card A
  makes card B stale.
- `crates/engine/src/credentials.rs:80` `save_key(provider_id, key)` accepts
  any id, so keys for draft providers need no change there.

Proto / RPC / doc:

- `crates/proto/src/entities.rs:81` `enum ChatScope { Normal, ModelSetup }`;
  `Chat.plan_mode: Option<ChatPlanState>` (293).
- `crates/rpc/src/lib.rs` ~60–88 model-setup method consts + docs; 198–215
  Plan Mode consts.
- `crates/doc/src/parts.rs`: `MessagePart` incl. `PlanApproval` with
  `PlanApprovalState` (226–249); `id()` / `byte_len()` must cover every
  variant. `crates/doc/src/schema.rs` 271 (serialize) / 358 (parse)
  `planApproval`. Unknown kinds degrade in older builds by rendering a `text`
  field — **new card parts carry no `text` field** so an old build shows
  nothing rather than garbage.

UI:

- `crates/ui/src/settings/providers/setup.rs` (2303 lines): the whole AI tab
  (`ai_tab`, `setup_key_request_card`, `setup_composer`,
  `setup_review_panel`, `apply_setup_proposal` → APPLY then
  `bump_provider_catalog` + reload, `settle_setup_key_request`, tests).
- `settings/providers/add_dialog.rs`: `enum AddProviderTab { Manual, Ai }`
  (18), tab strip (124–170), `open_add_dialog` (283).
- `settings/providers/test_support.rs`: `FakeSetupEngine`.
- `settings/providers.rs:49` `ProvidersPageEvent { Error }`; the shell
  subscribes at `shell.rs:1764`. Settings is a shell route
  (`Route::Settings`, `shell.rs:362`); the new-chat canvas is
  `NavEntry::Chat("")`.
- `transcript/plan_card.rs` (344 lines) — card template.
- Plan Mode draft/chip/slash: `pickers.rs:163` + `composer.rs:112`
  (`plan_mode_draft`), `pickers/mode.rs:123–290` (`plan_label`, `plan_chip`,
  exit), `composer/send.rs` (164–172 slash, 277 new chat enters the mode),
  `composer/popups.rs:744`, `composer/slash.rs` (`parse_plan`).

Tests: `crates/engine/tests/model_setup_rpc.rs` (795 lines) drives the
dialog flow through `START_MODEL_SETUP_CHAT`; it is rewritten, not patched.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Drift check | see header | Empty, or reviewed |
| Format check | `cargo fmt -p holt-ui -p holt-engine -p holt-doc -p holt-theme -p holt-proto -p holt-rpc -p holt-syntax -p holt -- --check` | Exit 0 |
| Doc tests | `cargo test -p holt-doc` | All pass |
| Engine tests | `cargo test -p holt-engine` | All pass |
| Mode tests | `cargo test -p holt-engine --test provider_mode_rpc` | All pass |
| UI tests | `cargo test -p holt-ui` | All pass |
| Lint | `cargo clippy --workspace --all-targets` | No new warnings vs baseline |
| Workspace | `cargo check --workspace && cargo test --workspace` | Exit 0 |

`cargo fmt --all` is broken here (vendored `vendor/gpui` workspace root);
always use the per-package form above. Record the clippy warning baseline at
the planned commit before Step 1.

## Scope

**In scope**:

- `crates/doc/src/{parts,schema}.rs`
- `crates/proto/src/entities.rs`, `crates/rpc/src/lib.rs`
- `crates/engine/src/{agent,rpc,plan_mode,provider_settings,store}.rs`,
  `crates/engine/src/tools/model_setup.rs`, new
  `crates/engine/src/provider_mode.rs`
- `crates/engine/tests/model_setup_rpc.rs` → renamed
  `crates/engine/tests/provider_mode_rpc.rs`; `crates/engine/tests/common/*`
  only if a fixture is needed
- `crates/ui/src/settings/providers/{setup,add_dialog,test_support}.rs`,
  `crates/ui/src/settings/providers.rs`, `crates/ui/src/shell.rs`
  (event wiring only), `crates/ui/src/transcript/**` (new
  `provider_card.rs` + model/render hooks), `crates/ui/src/pickers{.rs,/mode.rs}`,
  `crates/ui/src/composer{.rs,/send.rs,/popups.rs,/slash.rs}`
- `CONTEXT.md`, `ARCHITECTURE.md`, `docs/adr/0030-*.md`, `docs/adr/0031-*.md`
  (status notes only), `plans/README.md`

**Out of scope**:

- `vendor/**`, the `pi-core-rs` checkout.
- ADR-0029's invariant: no tool may write the catalog; `ApplyModelProposal`
  stays the only writer and writes the change exactly as stored.
- The key value path: it never enters History, a tool argument, a tool
  result, or the Transcript. No change to `credentials.rs`.
- The Manual tab of the add dialog, the provider/model record forms.
- Renaming `model_setup.rs` or the tool names `model_proposal` /
  `request_provider_key` (the prompt and existing tests key on them).
- Auto-exit of the mode after a write, a mode for subagents, MCP in the mode.

## Git workflow

- Branch `feat/010-provider-mode` off `main`.
- One commit per step, conventional and crate-scoped, e.g.
  `feat(doc): model proposal and key request card parts`.
- Every commit must build and pass `cargo test -p <touched crates>`.
- Do not push, merge, or open a PR unless the operator asks.

## Steps

### Step 1: Card parts in the doc schema

In `crates/doc/src/parts.rs` add:

```rust
MessagePart::ModelProposal {
    id: String,              // part id
    proposal_id: String,     // engine StoredProposal id
    summary: String,
    changes: serde_json::Value, // change_view() rows, display only
    state: ProposalCardState, // Pending | Written | Discarded | Superseded
}
MessagePart::KeyRequest {
    id: String,
    provider_id: String,
    provider_name: String,
    destination: String,     // base URL shown to the user
    state: KeyCardState,     // Pending | Saved | Dismissed | Superseded
}
```

Cover both in `id()` and `byte_len()`. In `schema.rs` serialize/parse them as
kinds `modelProposal` / `keyRequest` with camelCase fields and **no `text`
field**; unknown states parse as `Superseded` (inert) rather than failing the
entry. Add round-trip tests next to the `planApproval` ones, plus one that an
old-build-shaped entry containing these kinds still parses the surrounding
parts.

Verify: `cargo test -p holt-doc`.

### Step 2: Serializable proposals and a per-provider staleness gate

In `model_setup.rs`:

- Derive `Serialize`/`Deserialize` on `CatalogChange`
  (`#[serde(tag = "action", rename_all = "camelCase")]`, variant names
  matching the tool's `action` strings). `CoreModel` / `CustomProvider`
  already serialize — confirm; if either does not, STOP.
- Replace `StoredProposal.baseline: ProviderSettingsSnapshot` with a
  `baseline: BaselineSlice` — for each provider id the proposal touches
  (`CatalogChange::provider_id`), that provider's entries from
  `custom_models`, `model_records`, `custom_providers`, `hidden_models`.
  Add `fn slice(&ProviderSettingsSnapshot, &BTreeSet<String>) -> BaselineSlice`.
- `apply_stored`: read the live snapshot `S`; if `slice(S, touched) !=
  proposal.baseline`, fail with the existing "provider settings changed since
  this proposal was created" message; else re-run `build_proposal` on `S`
  (revalidate), `apply_changes` onto `S`, and call
  `replace_if_unchanged(&S, next)` — the whole-snapshot CAS now only guards
  the read→write window, not the proposal's lifetime.

Unit tests (in `model_setup.rs`): two proposals on different providers —
writing one leaves the other appliable; two on the same provider — writing
one makes the other fail stale; a change to an untouched provider does not
stale a proposal.

Verify: `cargo test -p holt-engine model_setup`.

### Step 3: Per-chat Provider Mode state file

New `crates/engine/src/provider_mode.rs`:

```rust
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct ProviderModeFile {
    proposals: Vec<StoredProposal>,               // pending only, ≤ PROPOSAL_CAP
    key_request: Option<PendingKeyRequest>,
    approved_key_destinations: BTreeSet<(String, String)>,
}
```

- Path `<data_dir>/provider-mode/<chatId>.json`; write atomically
  (temp + rename, like the other store files) after every mutation of the
  three `ChatRuntime` fields; missing/corrupt file → default + `warn!`.
- `ChatRuntime::load` (reached from `AgentRuntime::chat`, agent.rs:802)
  seeds `proposals`, `key_request`, `approved_key_destinations` from it.
  `approved_key_destinations` becomes chat-scoped and persisted (ADR-0037
  amends ADR-0031).
- `remove_chat` (agent.rs:839) deletes the file.
- Route every existing mutation site (`store_proposal`, `discard_stored`,
  `apply_stored`, key request set/take, approval insert) through one
  `ChatRuntime::save_provider_mode()` helper.

Tests: state survives dropping and reloading the runtime; `remove_chat`
deletes the file.

Verify: `cargo test -p holt-engine`.

### Step 4: Mode state, RPCs, and Turn shaping

- `proto`: `Chat.provider_mode: bool` (`#[serde(default)]`,
  skip-serializing-if false); `ProviderModeState { active: bool }`.
- `rpc` crate: `ENTER_PROVIDER_MODE`, `EXIT_PROVIDER_MODE`,
  `GET_PROVIDER_MODE` (`{chatId}` → `ProviderModeState`), documented like the
  Plan Mode trio.
- `engine/rpc.rs`: `enter_provider_mode` / `exit_provider_mode`, idempotent,
  persisted on the chat row and broadcast like `enter_plan_mode`. Entering
  Provider Mode while planning calls the existing `exit_plan_mode` path
  first; `enter_plan_mode` likewise exits Provider Mode first. Exiting does
  **not** discard pending proposals — cards stay writable (the RPCs do not
  check the mode).
- Admission (rpc.rs ~878–1017): snapshot `provider_mode = row.provider_mode`
  next to `planning`; `AgentRun.provider_mode` replaces `setup_scope`.
- `provider_mode.rs`: `pub(crate) fn provider_mode_block(web_search: bool) ->
  String` — adapt `setup_system_prompt`'s workflow (research → resolve →
  propose → stop) to an **appended** block, and add: (a) request the key as
  soon as the docs say the endpoint needs auth, not only after a 401; (b)
  a provider that does not exist yet is addressed as a draft
  `{id, name, baseUrl, defaultApi}` for the key request and inquiry probe;
  (c) after proposing, stop and let the user write — never claim it is
  written. `pub(crate) fn provider_mode_tool_allowed(name) -> bool` —
  `web_fetch`, `web_search`, `model_proposal`, `request_provider_key`.
- `agent.rs`: delete the prompt replacement at 1912; in the toolset block
  (2385) `else if provider_mode { tools.retain(provider_mode_tool_allowed on
  web_*) ; push both catalog tools }`; MCP guard (2431) uses
  `!provider_mode`; after the Plan Mode append (2448), `if provider_mode {
  system_prompt.push_str(&provider_mode_block(..)) }`. Outside the mode
  neither catalog tool is mounted.

Tests (`provider_mode_rpc.rs`, new): enter/exit round-trip and persistence;
Plan ↔ Provider mutual exclusion; a mode Turn's request carries exactly the
four tools and the workspace prompt plus the block; a normal Turn carries
neither catalog tool; a turn admitted before exit keeps its snapshot.

Verify: `cargo test -p holt-engine`.

### Step 5: Cards in the transcript

In the `ToolExecutionEnd` handler (agent.rs:2092), after `resolve_tool_part`:

- `model_proposal` with `details.proposalId` → append a
  `MessagePart::ModelProposal` built from the stored proposal
  (`proposal_views` / `change_view`) to the same assistant entry, in both
  `base_parts` and the transcript entry, then `persist_entry` (full-line
  re-append, ADR-0032).
- `request_provider_key` with `details.providerId` → append a
  `MessagePart::KeyRequest` from the pending request.
- Superseding, in `provider_mode.rs`, modeled on `settle_plan_cards`
  (plan_mode.rs:166): `stamp_proposal_cards(chat, pred, state)` and
  `stamp_key_cards(...)`.
  - New proposal: any pending card whose proposal touches a provider the new
    one touches → `Superseded`, and its `StoredProposal` is dropped.
  - `PROPOSAL_CAP` eviction → the evicted card → `Superseded`.
  - New key request → the prior pending key card → `Superseded`.
- `APPLY_MODEL_PROPOSAL` success → card `Written` (then the existing
  `refresh_catalog_windows`); `DISCARD_MODEL_PROPOSAL` → `Discarded`;
  `SETTLE_PROVIDER_KEY_REQUEST` → `Saved` / `Dismissed`. Apply failure leaves
  the card `Pending` and returns the error for the card to show.
- An apply/discard for a proposal id that is gone (superseded, evicted)
  returns an error and stamps the card `Superseded` so a stale Pending card
  cannot linger.

Tests: a proposal call yields one pending card with the proposal id; Write
stamps `Written` and the catalog changes; Discard stamps `Discarded`; a
second proposal for the same provider supersedes the first (card + store);
different providers stay independent; the cap evicts and stamps; card
states survive a runtime reload (transcript + state file agree).

Verify: `cargo test -p holt-engine`.

### Step 6: Draft providers for keys and probes

- `request_provider_key`: optional `provider` param
  `{id, name, baseUrl, defaultApi}`. Resolution order becomes: stored custom
  provider → newest pending proposal's planned provider → catalog provider →
  the draft. A draft must pass the same public-host gate as a planned probe
  (`planned_probe_problem` / `ip_is_public`); refused → tool error, no card.
- `model_proposal` inquiry mode: accept the same `provider` draft for the
  probe target. The key rides only when
  `(provider_id, baseUrl)` ∈ the chat's `approved_key_destinations`;
  loopback/private drafts stay refused, key or not.
- Update both tool descriptions (`KEY_REQUEST_DESCRIPTION`,
  `PROPOSAL_DESCRIPTION`).

Tests: draft key request → settle saves under the draft id and approves the
destination → an inquiry probe on the draft carries the key; a draft with a
different baseUrl does not; a loopback draft is refused; the key value never
appears in the transcript, History, or any tool result (search the persisted
files for the test key string).

Verify: `cargo test -p holt-engine`.

### Step 7: UI — chip, `/provider`, cards, Settings entry

- `pickers.rs` / `composer.rs`: `provider_mode_draft: bool` beside
  `plan_mode_draft`; setting one clears the other.
- `pickers/mode.rs`: `provider_label` + `provider_chip` (label "Provider",
  hover ×) mirroring `plan_chip`; × calls `EXIT_PROVIDER_MODE` or clears the
  draft.
- `composer/slash.rs` + `send.rs` + `popups.rs`: `/provider` enters
  (`/provider off` exits, `/provider <text>` enters and sends), mirroring
  `/plan`; a new chat sent with the draft calls `ENTER_PROVIDER_MODE` before
  the first Turn (send.rs:277 pattern).
- `transcript/provider_card.rs` (from `plan_card.rs`):
  - Proposal card: summary + change rows (reuse the row rendering from
    `setup_review_panel`, moved here), Write / Discard while Pending, a
    single state line otherwise. Write → `APPLY_MODEL_PROPOSAL` then
    `bump_provider_catalog` so model pickers refresh; an error renders under
    the card ("Catalog changed — ask to re-propose" for the stale case).
  - Key card: provider name, destination, masked secret input (move the
    input handling from `setup_key_request_card`), Save / Dismiss while
    Pending. The input entity is keyed by part id and dropped on settle; the
    value goes straight to `SETTLE_PROVIDER_KEY_REQUEST` and is never stored
    in UI state beyond the input.
  - Wire both into `transcript/{model,render}.rs` like `PlanApproval`.
- Settings: `AddProviderTab::Ai` is removed; the add dialog keeps Manual,
  and the page gets an "Add with AI" button that emits
  `ProvidersPageEvent::StartProviderChat`; the shell (`shell.rs:1764`
  subscription) navigates to the new-chat canvas (`NavEntry::Chat("")`) with
  `provider_mode_draft = true`.
- Delete the AI tab from `setup.rs` (everything except helpers the cards
  moved) and the setup half of `test_support.rs`; if `setup.rs` ends empty,
  remove the module.

UI tests: chip shows for the draft and the chat row; `/provider` parse forms;
a Pending proposal card renders Write/Discard and a Written one does not;
the Settings button emits the event.

Verify: `cargo test -p holt-ui`.

### Step 8: Remove the dialog-era surface

- `rpc` crate + `engine/rpc.rs`: delete `START_MODEL_SETUP_CHAT` /
  `start_model_setup_chat`, `LIST_MODEL_PROPOSALS`,
  `GET_PROVIDER_KEY_REQUEST`.
- `ChatScope::ModelSetup` stays in the enum (decodable); on engine startup
  delete every chat row with that scope through `remove_chat` (one pass in
  the chats load path), and drop the admission `setup_scope` read.
- Delete `setup_system_prompt` if Step 4 left it unused.
- Finish the test rewrite: port every still-meaningful case from
  `model_setup_rpc.rs` (stale rejection, unknown ids, discard, key settle /
  dismiss / replace, settle-without-pending, stored-probe key, loopback
  refusal, "normal chats reject the tools") into `provider_mode_rpc.rs` and
  delete the old file. Add: a legacy `ModelSetup` chat row is removed on
  startup.

Verify: `cargo check --workspace && cargo test --workspace`;
`grep -rn 'START_MODEL_SETUP_CHAT\|GET_PROVIDER_KEY_REQUEST\|LIST_MODEL_PROPOSALS\|setup_scope' crates`
returns nothing.

### Step 9: Docs

- `CONTEXT.md`: replace the Setup chat entry with **Provider mode**; update
  Model proposal and Key request (cards in the transcript, chat-scoped
  approvals, draft providers).
- `ARCHITECTURE.md` (~44–70, ~555–567): the model-setup description → Provider
  Mode. This file had unrelated uncommitted edits at planning time — edit
  only these sections.
- `docs/adr/0030-*.md`: status note "Superseded by ADR-0037".
  `docs/adr/0031-*.md`: amendment note pointing to ADR-0037 (card in the
  transcript, chat-scoped persisted approvals, draft providers).
- `plans/README.md`: update the 010 row.

### Step 10: Verification gates

Run every command in the table. Then a manual smoke run of the app:
enter via `/provider`, add a provider whose docs require a key, confirm the
order key card → save → probe → proposal card → Write, the model appears in
the picker, the mode stays on, restart the app and the chat's cards keep
their state, and a Pending card from before the restart still writes.

## Test plan

- Doc: part round-trips, no `text` field, unknown-state tolerance.
- Engine unit: baseline slice staleness (independent vs overlapping
  providers), state file round-trip, supersede rules.
- Engine integration (`provider_mode_rpc.rs`): mode RPCs and mutual
  exclusion, toolset/prompt shape in and out of the mode, card lifecycle,
  persistence across reload, draft key flow, key-value leak search, legacy
  scope cleanup, and the ported `model_setup_rpc.rs` cases.
- UI: chip, slash parse, card render states, Settings event.

## Done criteria

- [ ] A normal chat enters Provider Mode via `/provider` or Settings'
  "Add with AI" (the chip, like the Plan chip, only shows the mode), and
  exits via the chip's × or `/provider off`.
- [ ] Mode Turns keep the workspace prompt, append the block, and mount
  exactly `web_fetch`/`web_search`/`model_proposal`/`request_provider_key`;
  non-mode Turns mount neither catalog tool.
- [ ] Proposals and key requests render as transcript cards; Write / Discard
  / Save / Dismiss stamp them; superseding follows ADR-0037.
- [ ] Cards and their engine state survive restart; deleting the chat
  deletes `provider-mode/<chatId>.json`.
- [ ] Writing one proposal does not stale an unrelated one.
- [ ] A key can be requested for a draft provider before any proposal; the
  key value never appears in History, tool args/results, or the Transcript.
- [ ] `StartModelSetupChat`, `ListModelProposals`, `GetProviderKeyRequest`,
  and the dialog's AI tab are gone; legacy `ModelSetup` rows are deleted on
  startup.
- [ ] CONTEXT.md, ARCHITECTURE.md, ADR-0030/0031 notes updated.
- [ ] fmt, clippy (no new warnings), `cargo test --workspace` green;
  `git diff --check` clean.

## STOP conditions

- `CoreModel` or `CustomProvider` cannot derive `Serialize`/`Deserialize`
  without changing `pi-core-rs`.
- The `ToolExecutionEnd` handler cannot reach the chat runtime or the
  current assistant entry id, so cards cannot be appended to the requesting
  message.
- Any design point would require a tool to write the catalog, or the key
  value to pass through a tool argument/result, History, or the Transcript.
- Adding the two part kinds breaks parsing of existing transcripts, or an
  old build would render them as visible text.
- Plan Mode and Provider Mode cannot be made mutually exclusive without
  changing Plan Mode's card semantics.
- A verification command fails twice after a reasonable targeted fix.

## Maintenance notes

- "Provider mode" is a working name; a rename touches the proto field, the
  three RPC consts, the chip label, the slash command, and CONTEXT.md —
  keep them in one commit.
- The narrowed staleness gate relies on `CatalogChange::provider_id`
  covering every provider a change reads or writes; a future change kind
  that touches two providers must return both.
- A key saved for a draft provider that is never written stays in the
  credential store (already true under ADR-0031); a cleanup affordance is a
  separate change.
- Reviewers: check the key path end to end (UI input → settle RPC →
  credentials only), that exiting the mode mid-Turn does not change the
  running Turn's toolset, and that stamping and the state file cannot
  disagree after a crash between the two writes (the transcript card is
  display; the state file is truth — an apply on a missing proposal stamps
  `Superseded`).

## Deviations recorded after execution

- The proposal baseline is a sha256 fingerprint of the touched providers'
  slice, not a stored `BaselineSlice`: record headers can be secret, so the
  slice must not be persisted. Recorded in ADR-0037.
- The `ModelProposal` part carries `lines: Vec<String>` (one plain line per
  change) instead of `changes` as `change_view()` JSON; the card renders the
  lines as text, and `change_view`/`proposal_views` went with the review
  panel.
- The engine carries stamped card states onto a running Turn's rebuilt
  entry (`carry_card_states`), and a settle with nothing pending stamps stale key
  cards superseded — both keep cards from reading pending after the engine
  has settled them.
