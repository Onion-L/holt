# Toolset mechanics

What a Turn can do: the assembly that mounts the built-in file/shell tools onto the local machine, holt's own tools (directory listing, in-process content search, web fetch and search, cross-chat reads, the question card), and the MCP pool that turns servers' tools into ordinary agent tools. Mechanisms only — which tools a given Turn gets is policy (see permission-gate, modes, goals-routines).

Not here: the permission gate that audits mutating calls (see permission-gate); the mode-narrowed toolsets and catalog-setup tools (see modes); delegation (see subagents); how tool events reach the transcript (see persistence).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Execution environment | `LocalExecutionEnv` satisfies pi-core's `FileSystem + Shell` contract rooted at the chat's cwd; the assembly hands each harness tool the shared `ExecutionToolContext` its `execute` downcasts for. The read tool's visual pipeline runs the bounded image codec (decode caps, orientation, size reduction — source files are never rewritten) | [`crates/engine/src/tools.rs`](../../crates/engine/src/tools.rs), [`images/codec.rs`](../../crates/engine/src/images/codec.rs) |
| Built-ins | pi-core's `read` / `write` / `edit` / `bash`, mounted onto the local machine with the user's login shell; write and edit targets record into the Turn's change-set attribution | `tools.rs` |
| Content search | The `grep` tool runs ripgrep's crates in process — the agent's content search keeps its ignore-respecting traversal (deliberately narrower than the composer's path search) | [`tools/grep.rs`](../../crates/engine/src/tools/grep.rs) |
| Web fetch | One HTTP(S) URL per call: 5 MB download cap (declared or streamed), 30 s request timeout, bounded redirects, the text handed to the model capped with an in-band truncation notice, backend error text clamped. Neither web tool enters the permission gate — fetching reads a page the way `read` reads a file | [`tools/web_fetch.rs`](../../crates/engine/src/tools/web_fetch.rs) |
| Web search | The user-chosen backend resolved once per Turn admission (keyless default active before any setup; off means the tool is simply absent, never erroring). One adapter per built-in over a shared transport scaffolding — whole-exchange budget, cancellation race — plus user-written custom definitions (a request template with JSON Pointers, or one MCP tool) read fresh from disk, never failing startup | [`tools/web_search/`](../../crates/engine/src/tools/web_search), [`web_search_settings.rs`](../../crates/engine/src/web_search_settings.rs) |
| Cross-chat reads | `read_chat` gives an agent another Chat's user-visible transcript — workspace-scoped, validated references, never the model-facing History | [`tools/read_chat.rs`](../../crates/engine/src/tools/read_chat.rs) |
| MCP pool | An app-scoped, lazily-started pool over the device-level `mcp.json`: enabled servers connect on demand at Turn start, list once, and the wrapped set is frozen into the running Turn. A server that fails to start, connect within its startup timeout, or answer its list is skipped for that Turn with a log line and retried lazily on the next — no background restart loop. Every listed tool becomes an ordinary agent tool named `mcp__<server>__<tool>` under hard caps (description bytes, joined result characters, list pages) | [`crates/engine/src/mcp/mod.rs`](../../crates/engine/src/mcp/mod.rs), [`mcp/config.rs`](../../crates/engine/src/mcp/config.rs) |
| Deferred schemas | Main-chat Turns also mount an `mcp_tools` loader and append a synthetic call/result pair of it to every LLM request marking the mounted set — providers with deferred-tool support keep the schemas out of static context until the model loads them; providers without see the full declarations. The pair never persists to the transcript. A loaded tool's later calls are immediate | `mcp/mod.rs` |
| Call plumbing | `tools/call` forwards under the server's call timeout; a dead connection settles its calls as error results the model reads. The child environment is sanitized (credential-shaped variables stripped unless the server's `env` sets them) and config values expand variables at run time | `mcp/mod.rs`, `mcp/config.rs` |

## Runtime path

1. **Turn admission** resolves the run's toolset: the always-on core, the mode-narrowed set, or the delegation-free planning set — then the MCP snapshot joins (main-chat Turns only).
2. **Mount**: each tool is wired with the chat's `ExecutionToolContext` (cwd, attribution, cancellation).
3. **A call arrives** through the loop, passes the gate's predicate if mutating, and executes against the environment — a file write records its path into the Turn's attribution set.
4. **A web search admission** reads the active backend once; no active entry leaves the tool unmounted for that Turn.

## Constraints, state, and failure behavior

- **Everything is bounded**: download, text, timeout, redirect, description, result, and page caps on the web and MCP paths; the image pipeline's decode and pixel ceilings. A bound is a typed tool error or an in-band truncation notice, never a silent loss and never a Turn failure.
- **Lazy means lazy**: nothing connects at app startup; a failed server costs one Turn its tools, never the Turn itself.
- **MCP calls are presumed mutating** by prefix (see permission-gate); the gate, not the pool, decides.
- **Subagents, planning Turns, and Provider Mode Turns mount no MCP tools** — the loader and pool serve main-chat Turns only.
- **A dead or hung server surfaces as content**: the model reads an error tool result and can route around it.

## Verification

```sh
cargo test -p holt-engine tools
cargo test -p holt-engine mcp
cargo test -p holt-engine --test all mcp
```

The suites cover the execution environment contract, the grep traversal, web-fetch caps and error taxonomy, the search adapters' result mapping and the custom-definition reader, MCP's strict startup parse with last-good hand-edit tolerance, the lazy pool and its caps, the deferred-schema loader, and the sanitized child environment.
