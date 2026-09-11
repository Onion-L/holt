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
