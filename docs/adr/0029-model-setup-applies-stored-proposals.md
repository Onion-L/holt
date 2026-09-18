# Model setup changes apply only through a stored, human-approved proposal

**Status:** the apply tool this ADR introduced is superseded by ADR-0030
(the review panel's RPC applies; the agent never holds an apply tool). The
proposal/apply separation and the gate invariant it established carry over.

The agent-side model setup surface is a read-only proposal tool that
validates a catalog change and stores it server-side, and an apply tool that
executes the stored change by id — never arguments supplied at apply time —
behind an approval that no permission mode exempts. API keys never enter a
chat (Settings is the only key path), and probing `GET {baseUrl}/models` is
read-only. This closes the prompt-injection path where a fetched page steers
the key's destination, and makes an accidental apply a no-op (there is no
stored proposal to execute). Jev review (ADR-0026's connection layer,
ADR-0027 removed the mode) may later judge proposals before the approval;
it is deliberately not part of this path.
