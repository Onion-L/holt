# Jev review mode removed; the Jev connection layer stays

Status: accepted

## Context

ADR-0026 shipped Jev review as a fourth permission mode. The mode is
being retired — the judgment surface it owned will return in a
better-shaped form later — but the TypeSafe connection it rode on
(the HTTP client with its question set and retry policy, and the
user-supplied key record with its settings surface) is exactly what
any future Jev-powered feature needs.

## Decision

The `jev-review` permission mode is removed end to end: the proto
variant, the gate branch and its escalation path, the picker tier,
the admission-time judge resolution, and the `jev-review` usage kind.
Stored `jev-review` configs read back as confirm-changes through the
existing unknown-value fallback. Two serialized additions survive for
old records: `ReviewJudge::Jev` (historical verdict chips keep naming
their judge) and the pending-gate `note` field (additive, no producer,
reusable).

The connection layer survives intact: the harness-written client in
the engine, the `jev.json` key record under the credentials pattern,
and the Get/Save/Reveal/Remove RPC quartet with its Settings group —
copy rewritten neutral, no feature named. A future feature mounts from
here without touching credentials or transport.

## Consequences

Old usage ledger lines carrying the `jev-review` kind are skipped at
load with a warning (the loader's standing leniency), so historical
Jev token rows drop out of Usage views. ADR-0026 is superseded by
this decision.
