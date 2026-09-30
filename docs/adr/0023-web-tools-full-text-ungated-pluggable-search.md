# Web tools: full text, ungated, user-chosen search backends

Status: Accepted

## Context

pi-core-rs deliberately ships no web tools (upstream's stance: the model
can curl), so holt's agent had no web capability. A five-way survey of
Claude Code, Codex, opencode, Kimi CLI, and pi
(`docs/research/web-tools-2026-09-10.md`) framed the choices: summarize
versus full text, provider-locked versus pluggable search, and where
fetch sits relative to the ADR-0014 permission gate.

## Decision

Two holt-native `AgentTool`s — `web_fetch` and `web_search` — mount in
`execution_tools_for_model`; pi-core-rs is unchanged. `web_fetch` takes
a URL and returns the full converted page (htmd for HTML→markdown,
pass-through for other text), never a model-summarized answer: no hidden
second model, no extra provider round-trip, output bounded like grep
(50 KB notice-truncation, 5 MB download cap, 30 s timeout, no cache in
v1). `web_search` is a thin internal `SearchBackend` trait — one query
in, a title/url/snippet list out — with Zhipu, Bocha, and Brave adapters;
reading a result page is `web_fetch`'s job, and no provider-locked
server-side search is ever wired in (holt is multi-provider; Claude
Code's non-configurable backend is the surveyed counter-pattern). The
backend is the user's choice, made in a Settings group backed by a
credentials-pattern `web-search.json` record; a configured same-vendor
provider key surfaces only as a hint, never a preselection, and an
unconfigured backend means the tool is simply not registered — absent
from the model's toolset, not erroring into its face.

Neither tool enters the ADR-0014 gate. Fetch is a read; a third
"network egress" tier would break the one-predicate model. The known
cost is accepted: in confirm-changes, ungated web tools plus ungated
file reads form an exfiltration channel (read a secret, encode it in a
URL or query). Per-call approval is theater against it — three of the
four surveyed agents ship ungated, and full-access bash already reaches
the network unchecked — so the answer, when it comes, is domain-level
allow/deny rules, not per-call prompts. For the same reason no SSRF/IP
filtering: fetching `http://localhost:3000` to check a dev server is a
first-class use case, and a filter would guard nothing bash doesn't
already expose.

## Consequences

- The sync-era `ToolCall::WebFetch` chip carries a `prompt` field; it
  stays forever `None` under full-text fetch.
- Explorer and Worker subagents mount both tools (Explorer's whitelist
  grows to `read | grep | read_chat | web_fetch | web_search`).
- Domain allow/deny rules for fetch are the recorded fast-follow; v1
  ships none, deliberately.

## Addendum: keyless default, several entries

Every keyed vendor made search a setup chore before it did anything. So
Exa joins as a fourth built-in kind that needs no key: its hosted MCP
endpoint (`https://mcp.exa.ai/mcp`) answers one stateless JSON-RPC
`tools/call` of `web_search_exa` without a key or an `initialize`
handshake, and its text reply goes to the model as-is under the usual
result header (the `SearchBackend` result is either hits or text). With
no `web-search.json` at all, Exa is active — the same default opencode
ships. The known cost: out of the box, queries go to Exa.

A generic "any MCP server tool" kind was built and dropped before
release: it leaked a transport into a settings choice users make by
service, needed a server/tool picker, and bypassed the MCP gate and
filters. A service worth having becomes a built-in adapter or a custom
definition (below).

`web-search.json` holds a list of entries (id = kind) with at most one
active; saving an entry activates it, removing the active one turns the
tool off rather than promoting another behind the user's back, and
`SetActiveWebSearchBackend` with a null id turns search off and keeps
the entries. Off persists — a reload never falls back to the default.
The earlier single-record file still loads as one active entry.

In Settings the group is one "Search service" dropdown: Off, then each
built-in in engine order. A keyless or configured service switches on
pick; a keyed one without a key shows the key field first (Cancel backs
out) and its save activates it. The active keyed service shows its key
field with reveal, Save, and Remove. The same-vendor provider-key hint
is gone: it read as "the provider key is used" while the records stay
independent.

## Addendum: user-defined backends

Users who want a service Holt does not ship define it in
`search-backends.json` next to `web-search.json` (format in
`docs/search-backends.md`). The earlier objection to a fixed REST protocol
was that no real service fits one; a definition instead describes the
service's own shape — a request template with `{query}`, `{count}`,
`{apiKey}` and JSON Pointers into the reply (`type: "http"`), or one tool
on an MCP server with the `mcp.json` connection fields (`type: "mcp"`).
Every definition becomes a `SearchBackend`, so the picker, key flow, and
tool are unchanged; the transport never reaches Settings, only the name.

The definitions are a separate file because `web-search.json` is
engine-written (a hand edit would be lost on the next save), holds keys
(0600, fails startup when malformed), and should not be shared. The
definitions hold none, so the file is read fresh on each settings read and
Turn admission and a mistake never fails startup: the whole file is
refused, the reason reaches Settings, and the built-ins keep working.
Entries of a kind nothing offers are therefore kept, key included, and just
left out of the view — so fixing a definition brings its entry back. A
custom `mcp` backend connects per Turn, outside the MCP pool: it exposes
only its one search tool, never the server's others.
