# MCP tool schemas defer through a loader tool

ADR-0034 mounts every enabled server's tools as ordinary agent tools with
their full description and input schema in the static context of every
request — the shape Cursor's harness postmortem and pi-core's
deferred-tools machinery both identify as the top avoidable token cost
once MCP servers multiply. Holt adopts the deferred shape: the schemas
leave the static context and load on demand, while tool names, approval
rules, the gate predicate, and the transcript surface stay exactly as
ADR-0034 defined them.

The mechanism is pi-core's, not ours. A tool result may carry
`added_tool_names`; at every request build `split_deferred_tools` keeps a
marked tool out of the declared set (serializing it per provider —
Anthropic `defer_loading`, OpenAI Responses additionalTools or tool
search — and falling back to full static declarations on providers
without support). A tool the assistant has already called *before* the
marker stays immediate, so used tools end up statically declared and
repeated calls need no reload.

Two Holt-owned pieces complete the wiring. A small `mcp_tools` loader
joins the main-chat toolset next to the Turn snapshot: called with no
arguments it returns the catalog (name plus a one-line description per
mounted tool); called with `names` it returns full definitions — capped
at eight per call, unknown names reported against the mounted set — and
marks them via `added_tool_names`. And the Turn's `convert_to_llm` hook
(ADR-0011) appends a synthetic pair to the LLM view of every request: a
loader call plus its catalog result whose `added_tool_names` marks the
whole snapshot, deferring every MCP schema from the first request. The
pair always sits after the conversation, so anything already called
precedes the marker and stays immediate; it is never persisted to the
transcript, so a Turn restart simply re-appends it and a mid-Turn
compaction returns uncalled tools to the deferred baseline.

The loader is not `mcp__`-prefixed, so the gate's presumed-mutating
predicate (ADR-0034) does not fire — it reads the Turn's own snapshot
and pauses nothing. Rejected alternatives: a per-provider conditional
bootstrap (the fallback already no-ops, and branching would couple the
converter to catalog internals); a compact static catalog in the system
prompt (duplicates the bootstrap result and busts the cache prefix on
every config change); and declaring deferral per tool in `mcp.json`
(user-tunable fragmentation of a harness invariant).
