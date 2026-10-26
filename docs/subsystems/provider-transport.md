# Provider transport and catalog

How Holt knows providers and models, and how keys reach them. The catalog is three layers — the compiled baseline under the hand-editable `provider-store.json` overlay under the live `provider-settings.json` layer — and every provider, model, resolution, and context-window answer reads the merge. Credentials live in their own store and enter the run as per-request snapshots.

Not here: the mode that *changes* the catalog from inside a chat (see modes); which tools a Turn mounts (see toolset-mechanics); the request transport itself (see run-loop).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Compiled baseline + overlay | `provider-store.json` is user-owned and hand-editable: providers match by id (a non-built-in id is dropped), models match by id within their provider (a file entry replaces the compiled record outright; file-only ids append), and a provider entry overrides only `baseUrl` and `headers` — `name`, `organizationId`, and `auth` always come from the compiled catalog. The merge is built once per boot and everything answers from it, so a file edit needs a restart. The file is never rewritten once it exists | [`crates/engine/src/provider_store.rs`](../../crates/engine/src/provider_store.rs) |
| Live layer | `provider-settings.json`, read per call — a write is visible immediately: bare custom model ids (metadata borrowed from a template, window withheld), complete model records (a same-id record replaces the catalog entry outright, and its own `baseUrl` rides the request path — how an endpoint fix reaches requests), custom provider definitions, and hidden model ids (listings only; chats already configured keep resolving). Reset means the compiled catalog under the overlay — the live layer is what reset deletes, and credentials survive it | [`crates/engine/src/provider_settings.rs`](../../crates/engine/src/provider_settings.rs) |
| Validation policy | Per entry, everywhere in both files: an entry that fails a check is dropped with a log line while the rest applies; an unparsable overlay file or unsupported version falls back to the compiled catalog — none of it fails boot. The live layer's unparsable file does fail startup | both stores |
| Credentials | `provider-credentials.json`: one key per provider id, mode `0600`, atomic replace, malformed fails startup loudly. Keys enter the agent loop as per-request snapshots; the UI sees a secret only through the dedicated reveal RPC | [`crates/engine/src/credentials.rs`](../../crates/engine/src/credentials.rs) |
| Adapter and eligibility | `ProviderAdapter` answers listings, resolution, context windows, and eligibility from the boot merge. Eligibility is api-key-shaped: complex-auth providers are never offered, and a key save for one is rejected. Sibling built-ins sharing an organization group into one row with concrete id variants; user-defined providers stand alone | [`crates/engine/src/providers.rs`](../../crates/engine/src/providers.rs) |
| The probe | `ProbeProvider` makes one `GET {baseUrl}/models` with the stored key: `ok` proves the endpoint (unauthenticated listings answer 200 regardless), `key_rejected` is the only verdict that says the key is wrong, and everything else is `unverifiable` — nothing either way | [`rpc/providers.rs`](../../crates/engine/src/rpc/providers.rs) |
| Logos | A custom provider's logo, normalized on upload, removed with its definition | [`provider_logos.rs`](../../crates/engine/src/provider_logos.rs) |
| UI cutaways | The Settings provider page (keys, model records, custom providers, reset), the model pickers, the configured badge | [`crates/ui/src/settings/providers/`](../../crates/ui/src/settings/providers) |

## Runtime path

1. **Boot** compiles the merge: baseline + overlay, one `Catalog`, handed to the adapter; the live layer loads as a store read on every call.
2. **A listing or resolution** reads the merge with the live layer applied and eligibility filtered; unconfigured rows still list — the UI's pickers filter them.
3. **A run** snapshots the resolved model and its credential at admission; the request path reads the model record's own `baseUrl` when one is set.
4. **A write** (key, record, provider, hidden ids) lands in its store and is visible immediately, except overlay edits, which wait for a restart.

## Constraints, state, and failure behavior

- **The overlay file is the user's**: Holt never rewrites it, tightens a broad mode to `0600`, and prefers falling back to the compiled catalog over failing boot. It holds no secret, but it decides which host the key is sent to — that is why the mode is pinned.
- **Plaintext http is loopback-only on the catalog layer**: the compiled catalog, the overlay, and model records under catalog ids require https. A user-defined provider is an explicit endpoint choice (ADR-custom-providers-accept-plaintext-http): its definition and the records filed under it accept plain `http` to any host. Boot drops definitions that shadow a catalog id or whose map key differs from their inner id before validating records; live custom lookups also exclude catalog ids, so an inert definition cannot redirect probe or key targets. A planned endpoint still faces the probe's SSRF gate until the definition is stored.
- **A complete record replaces, never merges**: per-field patching is not expressible in either file — records are whole.
- **Hiding is cosmetic**: a hidden model leaves listings only; resolution keeps working so a configured chat never breaks.
- **The probe's verdicts are honest about what they cannot prove**: only a 401/403 indicts the key; a transport failure accuses nobody.
- **There is no enable toggle**: `configured` derives from the credential record; eligibility is shape, not preference.

## Verification

```sh
cargo test -p holt-engine provider
cargo test -p holt-engine credentials
```

The suites cover the three-layer merge (replacement, appending, override limits, non-built-in drops), per-entry validation and fallback, live-layer immediate visibility and reset semantics, credential persistence and reveal, eligibility filtering and organization grouping, and the probe's three verdicts.
