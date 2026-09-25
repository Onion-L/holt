# Plan 011: Provider choice — resolve an organization to one provider

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW — adds one doc part kind and one tool inside Provider Mode; the
  ADR-0029 write path and the ADR-0031 key rules are untouched.
- **Depends on**: plan 010 (merged)
- **Planned at**: commit `07a2e86`, 2026-09-25

## Why

Users name an organization ("xiaomi"), which often carries several providers
(`xiaomi`, `xiaomi-token-plan-{ams,cn,sgp}`). The flow should need as little
from the user as possible, and the result must show plainly which provider
the change landed in. Decisions are in ADR-0037's organization paragraph.

## Steps

1. **Doc** (`crates/doc`): `ProviderRef {id, name, detail, configured}`;
   `ModelProposal` gains `targets: Vec<ProviderRef>` (serde default, so old
   cards decode); new `ProviderChoice {id, options, chosen, state}` part with
   `ChoiceCardState {Pending, Chosen, Superseded}`; schema kind
   `providerChoice`. Test: round-trip and old-card decode.
2. **Engine**:
   - `choose_provider {providerIds}` tool (Provider Mode only): 2–6 distinct
     catalog provider ids; options are filled from the catalog (display name,
     endpoint host, configured). Details → `ProviderChoice` card.
   - The proposal tool's details carry `targets` (display names from the
     batch's own provider definition, else the catalog).
   - `SettleProviderChoice {chatId, cardId, providerId}`: the card must be
     pending and list the id; stamps `Chosen`, queues
     "Use provider <id> (<name>).", reverts on queue failure.
   - A Provider Mode Turn start supersedes pending choice cards;
     `carry_card_states` carries choice state.
   - Prompt step 2: the auto-resolve order, then `choose_provider` + stop.
   - Tests in `tests/provider_mode_rpc.rs`: card options are engine-filled,
     unknown ids refused, settle stamps + queues, bad settles refused, a new
     Turn supersedes, proposal cards carry targets.
3. **UI**:
   - Choice card: one row per option (brand icon, name, id, host,
     configured badge); click → `SettleProviderChoice`.
   - Proposal card header: target icon + name + id; written state
     "✓ Written to <name>" + "Open in Settings", which opens Providers with
     that provider's organization expanded and variant selected.
   - Tests: rows built from parts; the click sends the settle.

Verification per step: `cargo fmt -p holt-engine -p holt-proto -p holt-rpc
-p holt-doc -p holt-ui -- --check`, `cargo clippy --workspace`,
`cargo test --workspace`, `git diff --check`.
