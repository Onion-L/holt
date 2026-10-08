# AGENTS.md

This repository ports `@earendil-works/pi-agent-core`, `pi-ai`, and
`pi-telemetry` v0.84.4 to standalone Rust. The goal is a Rust implementation
that can replace the TypeScript packages directly without requiring a Node.js
runtime. The TypeScript sources under `pi-core/` are the behavioral
specification and test oracle. All product code changes belong on the Rust side.

## Repository layout

- `pi-core/{agent,ai,telemetry}/` — read-only TypeScript source and tests.
- `src/` — Rust implementation. Keep module paths close to their TypeScript
  counterparts when that helps review, but do not force a one-file mapping when
  it makes the Rust API worse.
- `tsconfig.base.json` — shared TypeScript configuration used by the oracle.

Do not modify files under `pi-core/` to make the Rust port pass. Test-only
oracle scripts and generated fixtures should live outside `pi-core/`.

## Compatibility standard

The Rust implementation must preserve observable TypeScript behavior while
exposing an idiomatic Rust API.

Observable behavior includes:

- agent event order and payloads;
- state transitions;
- session JSONL format;
- tool names, schemas, validation, and output text;
- compaction and branch-summary results;
- retry and cancellation behavior;
- provider stream normalization and usage accounting.

When Rust and TypeScript disagree, reproduce the TypeScript behavior even when
it appears undesirable. Do not silently fix upstream behavior. If exact parity
is impossible because of an OS or runtime difference, add a focused test,
document the difference next to it, and keep the deviation as small as possible.

“Idiomatic Rust” applies to API shape, ownership, errors, traits, enums, and
async code. It does not permit observable behavior changes.

## Scope

All public behavior in these source trees is in scope:

- `pi-core/telemetry/src/`;
- `pi-core/ai/src/`, including every provider, Bedrock, provider APIs, model
  catalogs, authentication and OAuth, images, streaming, retries, validation,
  and compatibility helpers;
- `pi-core/agent/src/`, including agent core, harness, session JSONL, tools,
  compaction, branch summarization, search, and skills.

The migration is complete only when the public exports and observable behavior
of all three TypeScript packages have Rust equivalents. Implementation order may
prioritize core paths, but that does not remove later providers or modules from
scope.

The `node:sqlite` session backend remains out of scope because it is a separate
upstream package and is not part of the three source trees above. It can be
ported as a separate Rust crate later.

Port every applicable test from all three packages. A test may remain skipped
only when the corresponding TypeScript test also requires unavailable live
credentials, or when a documented platform limitation makes it inapplicable.
Keep skipped cases visible with a short reason so missing coverage is deliberate
rather than accidental.

## Porting workflow

Port in dependency order: telemetry → ai → agent core → harness.

For each module:

1. Read the TypeScript source and its tests together.
2. Identify observable inputs, outputs, state changes, and error cases.
3. Port the implementation.
4. Port every applicable test case, preserving its scenario, fixtures, and
   assertions rather than its TypeScript structure.
5. Add differential or golden tests for serialized output and event sequences.
6. Run the focused Rust tests, then the crate-wide checks.

A module is complete when its in-scope tests are ported and passing, required
goldens match, and no unexplained compatibility gap remains.

## Oracle and goldens

Use TypeScript output as the source of expected serialized data. Do not write
expected JSON or event payloads from memory.

- Put committed fixtures under `tests/goldens/<module>/`.
- Add a reproducible generator outside `pi-core/` for each new class of golden.
- Freeze timestamps, UUIDs, randomness, environment variables, and temporary
  paths used in golden output.
- Byte-compare formats whose bytes are part of the contract, especially session
  JSONL. For values without a byte-level contract, compare parsed structures.
- Account for field order, omitted fields, `null`, number formatting, escaping,
  and trailing newlines.
- A golden update must be produced by the TypeScript oracle, never by copying
  the Rust result into the expected file.

Provider SSE tests should preserve chunk boundaries, partial tool arguments,
thinking blocks, stop reasons, errors, and usage fields from the TypeScript
fixtures.

## Running the TypeScript oracle

The packages require Node.js 22.19 or newer. Tests use local source aliases;
live API tests should skip when credentials are absent.

```bash
cd pi-core/telemetry && npm install --ignore-scripts && npm test
cd pi-core/ai && npm install --ignore-scripts && npm test
cd pi-core/agent && npm install --ignore-scripts && npm test && npm run test:harness
```

Do not add API credentials merely to run the oracle. Do not update TypeScript
dependencies as part of a Rust port. If dependency installation changes oracle
behavior, stop and record the Node.js, npm, and resolved dependency versions.

## Rust verification

Run focused tests while developing. Before considering a module complete, run:

```bash
cargo test --all-targets
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Common parity traps

- Use the `uuid` crate with the v7 feature for upstream UUIDv7 values.
- Inject or freeze clocks and ID generators in deterministic tests.
- Preserve JSON field names, camelCase conventions, union representation, and
  omission rules.
- Match tool output and error strings exactly when they are part of the public
  behavior.
- Keep retry delays, attempt counts, cancellation points, and terminal events
  aligned with TypeScript.
- For Node-specific harness tools (`bash`, `read`, `write`, and `edit`), isolate
  unavoidable OS differences and test the shared behavior exactly.
