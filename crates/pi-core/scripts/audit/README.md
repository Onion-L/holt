# Export parity audit

`export-parity.mjs` keeps the Rust port honest against the TypeScript oracle's
public surface. It statically enumerates every export of
`pi-core/{telemetry,ai,agent}/src` (via the oracle's own `typescript` package)
and checks each name against the committed classification in
`export-parity.json`:

- `mapped` — a public Rust symbol with the recorded `rust` name exists under
  `src/`. The symbol list is re-verified by `tests/export_parity.rs` on every
  `cargo test` run.
- `partial` — the runtime capability exists in Rust but the TS public surface
  is only partly exposed; `reason` names the follow-up.
- `exception` — a TypeScript language, bundler, or schema-level construct
  with no Rust counterpart by design; `reason` documents the deviation.

Usage:

```bash
node scripts/audit/export-parity.mjs            # validate (exit 1 on gaps)
node scripts/audit/export-parity.mjs --update   # sync TS-side changes into
                                                # the manifest, preserving
                                                # classifications
node scripts/audit/export-parity.mjs --auto-map # fill `todo` entries that
                                                # unambiguously match a pub
                                                # Rust symbol
```

A new TypeScript export therefore fails the audit until someone records the
Rust decision for it — the same tripwire that keeps `MIGRATION.md` statuses
grounded. Statuses here and in `MIGRATION.md` are updated together.
