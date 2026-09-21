# MCP servers join as engine-owned clients behind the permission gate

Holt speaks MCP as a client only: `crates/engine` owns an app-scoped pool
of MCP connections (official `rmcp` crate; stdio + Streamable HTTP — no
SSE, no OAuth yet, static `headers` and `bearer_token_env_var` only), and
every `tools/list` entry is wrapped as a plain `AgentTool` named
`mcp__<server>__<tool>`. The UI keeps rendering `MessagePart`s and never
learns MCP exists; prompts, resources, elicitation, roots, and a reverse
server mode are out of scope by design.

The load-bearing decision inverts the gate's predicate. ADR-0014 makes
mutating a whitelist (`write|edit|bash`); MCP tools are instead presumed
mutating by prefix: in confirm-changes every `mcp__` call pauses behind
the ordinary Approval, auto-review's model pass judges them like any
mutating call, and full-access passes them through. An always-allow
records the exact two-level tool name, nothing broader. Rejected:
trusting `readOnlyHint` (the spec calls annotations untrusted) and never
gating (foreign code with zero audit surface — a stdio server sees
everything its process can see).

Lifecycle and resource ceilings: tools snapshot when a Turn starts — a
`listChanged` notification lands next Turn, the same semantics as a
permission-mode switch — and connections start lazily before the first
Turn that needs them. A server that fails to connect or dies mid-Turn is
skipped with a log line (its tool calls settle as error results the
model reads) and reconnects the same lazy way next Turn; there is no
background restart loop. A malformed `mcp.json` fails startup loudly
(credentials pattern: 0600, atomic replace). Context floods are capped:
tool results truncate at 100k chars, descriptions at 2KB, `tools/list`
follows pagination with a 100-page cap, and no per-server tool-count
limit — the `enabled_tools`/`disabled_tools` lists (deny wins) are the
intended throttle. stdio children inherit a sanitized environment:
credential-shaped variables (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`,
`*KEY*`, `*AUTH*`) are stripped unless the server's own `env` sets them
explicitly.

Servers live in `~/.holt/mcp.json` (`mcpServers`, strict unknown-key
error, `${VAR}`/`${VAR:-default}` stored unexpanded and expanded at run
time; unset with no default keeps the literal and warns) and are managed
from a Settings page over four RPCs: `GetMcpSettings`,
`SaveMcpServer` (upsert, validates the `[A-Za-z0-9_-]` name),
`RemoveMcpServer`, and `TestMcpServer` — a read-only probe that
connects, lists tools, reports the failure, and disconnects. No standing
status watch: on-demand status is what keeps startup lazy. Phase 1 also
keeps MCP tools out of subagents (their curated explorer/worker sets
would break the read-only invariant) and renders MCP calls as Unknown
transcript parts. Research basis: `docs/research/mcp.md`.
