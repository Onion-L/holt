# A Survey of MCP Integration Patterns in Coding Agents

> Survey date: 2026-09-10. This document is written for engineers and surveys how mainstream coding agents (including the closed-source but well-documented Claude Code / ZCode) integrate the Model Context Protocol (MCP) and let their agents use MCP tools. Background: Holt's agent loop comes from pi-core-rs, which currently has no MCP support at all; industry practice needs to be evaluated before deciding on an integration path. All conclusions are backed by source-level evidence wherever possible; volatile details (version numbers, defaults, behavior switches) are accurate as of the date consulted.
> Amended 2026-09-21: added Zed (§8.12) and "Config interop: importing other tools' MCP configs" (§10), and re-verified the state of Codex / Claude Code / pi (stale items are summarized in §10.6); the new citation numbers [Z*]/[I*] are listed in §12.

## 1. Conclusions first

- **Almost everyone is an MCP client**: apart from aider, which explicitly opts out, every surveyed coding agent ships a built-in MCP client; implementations overwhelmingly pick the official SDK — TS projects use `@modelcontextprotocol/sdk`, Rust projects (Codex, Goose, Amazon Q) use `rmcp`, the Go project Crush uses `github.com/modelcontextprotocol/go-sdk/mcp`, and the Python Kimi CLI uses `fastmcp` (wrapping the official `mcp` SDK). Only a tiny number of early implementations rolled their own protocol stack, and they are all migrating toward official SDKs.
- **Tool mapping is highly uniform**: every MCP tool is converted into an ordinary tool definition in the host agent (name + description + inputSchema + execute) and goes through the existing tool-call loop; nobody opens a separate execution channel just for MCP. The only divergence is namespacing: Claude Code / ZCode / Gemini CLI use the fully qualified names `mcp__server__tool` / `mcp_server_tool`, while Codex / Kimi use the bare `tool.name` (collisions are handled by server precedence or reported as errors).
- **Transport has converged on stdio + Streamable HTTP**: since spec 2025-03-26, Streamable HTTP replaces HTTP+SSE; SSE is already marked deprecated in Claude Code and deleted from the Codex source. Claude Code additionally supports WebSocket, and Goose supports unix sockets.
- **Config converges on "a blob of server definitions in JSON"**: the Claude-family shape (`.mcp.json`'s `mcpServers`) has become the de facto standard — Codex (TOML expressing the same fields), ZCode (`mcp.servers` + `.agents/mcp.json` compatibility fallback), Kimi, Continue, and Amazon Q (whose source comment self-describes as "mirror claude's config") all interoperate with or imitate this shape. Scope tiers are consistently user / project (workspace) / current session, plus a managed/plugin tier.
- **Permission gating comes in three tiers**: a server-level switch (enabled/disabled), a server-level approval mode (Codex's `default_tools_approval_mode`, Gemini's `trust`), and tool-level granularity (Claude's `mcp__server__tool` permission rules, Roo's `alwaysAllow`, Codex's per-tool `approval_mode`). "First use of a project-level server requires user confirmation" is a trust-boundary design shared by Claude Code and ZCode.
- **Lazy loading is the new battleground**: Claude Code enables Tool Search by default (injecting only tool names, fetching schemas on demand); Codex caps the paged catalog and caches it; Goose offers Code Mode (discovering/calling tools from JS). This directly answers the pi author's critique — a large MCP server's tool definitions can easily eat 7-9% of the context.
- **Zed takes a third path: hand-rolled protocol stack + bare tool names** (§8.12): hand-written JSON-RPC/MCP types (no rmcp), stdio + streamable HTTP + full OAuth (PKCE/DCR/keychain); the LLM sees bare tool names, with a server prefix added only on name collisions, while settings/approvals use the stable ID `mcp:<server>:<tool>`; eager startup, manual restart on failure, and no `tools/list` pagination (a known weakness); it reads neither `.mcp.json` nor any imported config.
- **"Importing someone else's config" is an explicit action in this ecosystem** (§10): ZCode scans four sources and imports via checkbox selection, Continue reads Claude-family JSON directly from a directory, and pi-mcp-adapter requires an explicit `/mcp setup` (host discovery defaults off); `.agents/mcp.json` is an unspec'd de facto convention, and the only spec-backed option is the plugin-level `mcp.json` from Agent Plugins 1.0.0. Also: Codex's default timeouts have changed to 30s/300s, and crush's config has migrated to the Bash `crushrc`.
- **Bidirectional capability is common but takes different shapes**: Claude Code (`claude mcp serve`) and Codex (the deprecated `codex mcp-server`, succeeded by app-server) can expose themselves as MCP servers; Goose only reverse-exposes its built-in extensions (`goose mcp <name>`) and speaks ACP to the outside; ZCode, Gemini CLI, and Kimi are pure clients.
- **OAuth is now standard equipment for remote servers**: implementations universally do OAuth 2.1 authorization-code + browser callback, honoring spec 2025-06-18's protected resource metadata / RFC 8707; both dynamic client registration (DCR) and pre-registered client_id are supported.

## 2. MCP protocol essentials (the parts relevant to coding agents)

### 2.1 Version and transport evolution

MCP encodes messages in JSON-RPC 2.0, with roles split into host (the LLM application), client (the connector inside the host), and server. [S1] Protocol versions are date-named; the evolution is as follows (consulted 2026-09-10):

| Version | Key changes |
| --- | --- |
| 2024-11-05 | First stable release; HTTP+SSE transport (two endpoints) |
| 2025-03-26 | **Streamable HTTP replaces HTTP+SSE**; JSON-RPC batching removed |
| 2025-06-18 | Structured tool output (`structuredContent`/`outputSchema`); server positioned as an OAuth Resource Server; mandatory RFC 8707 resource indicators; elicitation added; `MCP-Protocol-Version` header |
| 2025-11-25 | Extensions framework (SEP-2133), async Tasks, standardized tool-name format (SEP-986), icon metadata |
| 2026-07-28 | Current latest; Tasks / MCP Apps (in-session interactive UI) / Skills over MCP are all offered as opt-in extensions [S2] |

The current spec defines only two standard transports [S3]:

- **stdio**: the client launches the server as a subprocess; newline-delimited JSON-RPC flows over stdin/stdout; stdout carries only protocol messages, and logs go to stderr. Supported by every surveyed project.
- **Streamable HTTP**: the server exposes a single MCP endpoint; the client POSTs each message, and the server may return a single JSON or open an SSE stream; supports the `Mcp-Session-Id` session header, `Last-Event-ID` replay after disconnect, and the `MCP-Protocol-Version` header. Legacy HTTP+SSE servers gain client compatibility via "on 4xx POST failure, fall back to GET and wait for endpoint events" [S3].

MCP Apps (rendering interactive UI-bearing components into the conversation) is an extension in the 2026-07-28 spec; Goose and the Claude ecosystem have already followed with related capabilities. [S2]

### 2.2 The tools primitives

- `tools/list` supports cursor pagination; `tools/call` carries `name` + `arguments`; `isError: true` signals a tool execution error (as distinct from a JSON-RPC protocol error). [S4]
- A server declares the `tools.listChanged` capability at initialize and sends `notifications/tools/list_changed` when its tool list changes; clients should re-fetch — Claude Code, ZCode, and others do dynamic refresh on exactly this basis. [S4]
- A tool definition includes `name`/`title`/`description`/`inputSchema`/`outputSchema`/`annotations`; **the spec is explicit that annotations (readOnlyHint etc.) are untrusted**. A tool result may be `content` (text/image/audio/resource_link/embedded resource) plus `structuredContent`. [S4]
- Security recommendations: humans SHOULD be in the loop, tool input shown before invocation, results validated before being fed to the LLM, and call timeouts implemented. [S4]

### 2.3 OAuth

Since 2025-06-18, a server is a Resource Server under OAuth 2.1, pointing at the authorization server via protected resource metadata; clients must implement RFC 8707 resource indicators so tokens cannot be coaxed out by a malicious server. [S5] Every surveyed client (Claude, Codex, Goose, opencode, Crush, Kimi, Gemini CLI) implements the "401 → discover authorization metadata → authorize in the browser → receive code via local callback → store token → auto-refresh" flow; the differences are callback port management (fixed/random) and client registration (DCR / CIMD / pre-registered).

## 3. OpenAI Codex CLI

Rust, openai/codex, ~122.8k stars (2026-09-10). [S6]

### 3.1 SDK and internal structure

- Uses the official Rust SDK: **the `codex-rmcp-client` crate depends on `rmcp`**, with features covering `client`, `server`, `transport-child-process`, `transport-streamable-http-client/server`, and `auth`, paired with the `oauth2` crate. [S7] The early `experimental_use_rmcp_client` toggle has disappeared from the repo (zero GitHub code search hits) — rmcp is now the only implementation, no longer "experimental".
- MCP-related code is concentrated in `codex-rs/codex-mcp` (connection management, catalog, elicitation, OAuth), `codex-rs/rmcp-client`, and `codex-rs/config/src/mcp_types.rs`. [S6][S8]

### 3.2 Configuration (`config.toml`)

Top level of `config.toml`: `mcp_servers: HashMap<String, McpServerConfig>`. [S9] The complete field set of `RawMcpServerConfig` (deny_unknown_fields) in source: [S8]

```toml
[mcp_servers.chrome_devtools]
url = "http://localhost:3000/mcp"      # or command/args/env (stdio)
enabled_tools = ["open"]                # allowlist, applied first
disabled_tools = ["screenshot"]         # blocklist, applied second
default_tools_approval_mode = "prompt"  # auto | prompt | writes | approve
startup_timeout_sec = 20                # default 10
tool_timeout_sec = 45                   # default 60
enabled = true
required = true                         # init failure fails the whole startup
[mcp_servers.chrome_devtools.tools.open]
approval_mode = "approve"               # per-tool approval
output_token_limit = 30000              # per-tool output truncation
```

Other fields: `env_vars` (environment variable references), `bearer_token_env_var`, `http_headers`/`env_http_headers`/`http_headers_helper` (a local shell command generating headers dynamically), `supports_parallel_tool_calls`, `scopes`, `oauth { client_id, callback_url, callback_port }`, `oauth_resource`. Global keys include `mcp_oauth_credentials_store`, `mcp_oauth_callback_port/url`, and `mcp_optional_startup_grace_ms` (default 1000ms, the grace period granted to optional servers). [S8][S9]

> Re-verified 2026-09-21: default timeouts are now startup 30s / tool 300s, the `auth` enum gained `ema_auth`, and fields such as `startup_timeout_ms`, `environment_id`, and `omit_tools_from` were added; the transport enum is unchanged. See §10.2 for details. [I3][I4][I5]

### 3.3 Transport and approval

- The transport enum has **only two variants left, `Stdio` and `StreamableHttp`**; SSE has been deleted from the source. [S10]
- The approval model is `AppToolApproval = auto | prompt | writes | approve` with conservative merge semantics (`restrict_to`): when the server-level default merges with a per-tool value, if either side is `prompt` the result is `prompt`. [S8] This and Codex's own `approval_policy` (untrusted/on-request/never, etc.) are two orthogonal mechanisms — MCP tool approval is independent of shell command approval.

### 3.4 Exposing tools to the model

- **Bare tool names**: `parse_mcp_tool` uses `tool.name.to_string()` directly with no server prefix; same-name collisions across servers are resolved at the catalog layer by same-tier precedence rules ("A same-tier name collision and the final outcome after all precedence is applied"). [S11]
- For OpenAI model compatibility, an inputSchema missing `properties` gets an empty object backfilled; in the agent-plugin case, descriptions are truncated to 1000 bytes. [S11]
- `tools/list` follows pagination, capped at 100 pages / 2048 tools per server, and caches the tool catalog (`McpToolCatalogCache`); at startup, optional servers get the `mcp_optional_startup_grace_ms` grace period. [S12][S9]

### 3.5 CLI and OAuth

The `codex mcp` subcommand group: `list`, `get`, `add`, `remove`, `login`, `logout` (source enum). [S13] OAuth supports both DCR and CIMD: `codex mcp login <name> --oauth-client-registration cimd|dcr|auto`; for HTTP servers, `auth = "oauth" | "chatgpt"` (the latter restricted to ChatGPT first-party domains). [S8][S9]

### 3.6 Reverse direction: MCP server mode

`codex mcp-server` (stdio) is **deprecated**; the official recommendation is to switch to the Codex app server or the Codex plugin for Claude Code; in that mode only two tools are exposed: `codex` (taking prompt/approval-policy/sandbox/config parameters, one call per session) and `codex-reply`. [S14] The current main-branch CLI subcommand table no longer contains `mcp-server`; in its place is `[experimental] app-server`. [S15]

## 4. Claude Code and claude-agent-sdk

The CLI itself is closed-source; the anthropics/claude-code repo is documentation/issues only. Documentation site: code.claude.com; the following was consulted 2026-09-10.

### 4.1 Configuration and scopes

- CLI: `claude mcp add --transport http|sse|stdio <name> ...`, `add-json`, `list`, `get`, `remove`, `login/logout`, `reset-project-choices`; plus the in-session `/mcp` panel. [S16]
- Three scopes: local (default, stored under the project entry in `~/.claude.json`), project (`.mcp.json`, checked into the repo), user (top level of `~/.claude.json`). On same-name conflicts, **the higher-priority definition wins wholesale — no field-level merging**, with order local > project > user > plugins > claude.ai connectors; the organizational `managedMcpServers` outranks everything. Servers in the project scope require interactive confirmation on first use (the trust boundary). [S17]
- `.mcp.json` supports `${VAR}` and `${VAR:-default}` expansion (command/args/env/url/headers). [S17]

### 4.2 Transport

http (recommended; in JSON, `streamable-http` is an alias for `http`), sse (deprecated), stdio (arguments separated by `--`; the server side can read `CLAUDE_PROJECT_DIR`), ws (JSON config only, no OAuth support). Entries with a url but no type are parsed as stdio, error out, and are skipped. [S16]

### 4.3 Tool naming and permissions

- Tool names are `mcp__<server>__<tool>`; plugin servers are `mcp__plugin_<plugin>_<server>__<tool>`, with illegal characters replaced by `_`. [S18]
- Permission rules: `mcp__server` (whole server), `mcp__server__tool` (exact), `mcp__server__*`/`mcp__server__prefix_*` (globs in allow rules must carry the literal `mcp__<server>__` prefix), `mcp__*` (deny all MCP tools). Allow rules do not support a bare `mcp__*`. A tool's `_meta["anthropic/requiresUserInteraction"] = true` can force an approval prompt on every call, which even bypassPermissions cannot skip. [S19][S20]

### 4.4 Tool Search / lazy loading

On by default: at session start only tool names and server instructions are injected, and the model fetches definitions on demand via the `ToolSearch` tool (requires a Claude 4.5+ model supporting `tool_reference`). `ENABLE_TOOL_SEARCH` accepts `true`/`auto` (front-load everything when total definitions are < 10% of context)/`auto:N`/`false`; exemptions come from server-level `alwaysLoad: true` or tool-level `_meta anthropic/alwaysLoad`. Tool descriptions and server instructions are truncated to 2KB. There is no fixed per-server tool-count cap. [S21]

### 4.5 Timeouts, output, and lifecycle

- Startup timeout `MCP_TIMEOUT`; per-server `timeout` (ms) is a hard wall-clock cap per call; `MCP_TOOL_TIMEOUT` defaults to roughly 28 hours when unset; idle timeout `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` (remote 5 minutes, stdio 30 minutes); MCP calls exceeding 2 minutes in the main session are automatically converted to background tasks. [S22]
- Output beyond 10k tokens triggers a warning, with a default cap of 25k (`MAX_MCP_OUTPUT_TOKENS`); a tool's `_meta["anthropic/maxResultSizeChars"]` can relax it to 500,000 characters, with over-limit results spilled to disk and swapped for a file reference. [S23]
- Client runtime: v1 = MCP TS SDK 1.x, v2 = SDK 2.0 (supports the 2026-07-28 protocol revisions); supports `list_changed` dynamic refresh and exponential-backoff reconnection for remote servers (stdio does not reconnect). [S24]

### 4.6 Capabilities beyond tools

- **prompts → slash commands**: `/mcp__server__promptname (MCP)`, arguments space-separated. [S25]
- **resources → @ references**: `@server:protocol://path` is auto-fetched as an attachment. [S25]
- **elicitation**: form mode pops a dialog, URL mode opens the browser; an Elicitation hook can auto-respond. [S25]
- **roots**: `roots/list` returns the launch directory plus additional working directories, sending `notifications/roots/list_changed` on change. [S16]
- **plugins packaging MCP**: a plugin-root `.mcp.json` or inline `mcpServers` in `plugin.json`, supporting the `${CLAUDE_PLUGIN_ROOT}` placeholder; enabling a plugin connects automatically. [S18]

### 4.7 Reverse direction and the SDK

`claude mcp serve` turns Claude Code itself into a stdio MCP server exposing only Claude Code's tools (approvals are the external client's responsibility). [S26] claude-agent-sdk (TS/Python, open-source) embeds the same capability set: `query({ options: { mcpServers: {...}, allowedTools: ["mcp__server__*"] } })`; beyond stdio/http/sse it also supports SDK in-process servers (`createSdkMcpServer`). [S27]

## 5. Kimi CLI

MoonshotAI/kimi-cli, Python, ~11.3k stars (2026-09-10). [S28]

- **SDK**: `fastmcp==3.2.4` (a pyproject dependency), types from the official `mcp` package; a `fastmcp.Client` is created per server. [S29]
- **Config**: `~/.kimi/mcp.json`, top-level `mcpServers`, compatible with the Claude-family format; plus `kimi --mcp-config-file <path>` / `--mcp-config '<json>'` for ad-hoc injection. `kimi mcp add --transport http|stdio` plus the `list`, `remove`, `auth`, `test` subcommands. [S30]
- **Transport**: stdio + streamable HTTP; HTTP supports `--header` and `--auth oauth` (browser authorization, tokens stored under `~/.kimi/mcp-oauth/`). [S30]
- **Tool exposure**: **bare `tool.name`**, with the server name written into the description prefix ("This is an MCP (Model Context Protocol) tool from MCP server `X`"); the schema passes `inputSchema` through directly; tools are registered into the unified toolset and go through the normal agent loop. [S29]
- **Approval and timeouts**: all MCP tool calls go through `runtime.approval.request` human confirmation by default, auto-approved under YOLO/AFK modes; the per-call timeout comes from `mcp.client.tool_call_timeout_ms`. [S29]
- **Lifecycle**: connections happen asynchronously after the shell UI starts, with live status in the status bar; lazy loading is supported (`defer_mcp_tool_loading`). The in-session `/mcp` shows status. [S30]

## 6. ZCode

Z.ai's coding agent (desktop app + CLI), **closed-source**; on GitHub only the plugin marketplace `zai-org/zcode-plugins` exists. The following combines first-hand local files (`~/.zcode/cli/`, consulted 2026-09-10) with the official docs at zcode.z.ai. [S31][S32]

- **Config and scopes** (first-hand: the local `zcode-guide` plugin's SKILL.md): user is `~/.zcode/cli/config.json` → `mcp.servers`; workspace is `<repo>/.zcode/config.json` or `zcode.json` → `mcp.servers`; each scope's `.agents/mcp.json` (top-level `mcpServers`) serves only as a same-scope fallback. Same-name override order is **CLI → environment variables → user → workspace → system**, with user overriding workspace. Servers in all scopes are **auto-trusted and connected at session start** (early versions required manual authorization for workspace; since changed). The desktop app's status/repair entry point is Settings → MCP. [S31]
- **Schema**: stdio `{command, args, cwd, env, enabled, timeoutMs}`; http/sse `{url, headers, enabled, timeoutMs}`; `type` can be inferred from command/url; **the schema is strict — an unknown key discards the whole server**; `${...}` template variables are expanded only for plugin servers; default timeout 30000ms. [S31]
- **Plugin servers**: a plugin-root `.mcp.json` or the manifest's `mcpServers` field (confirmed first-hand by the local computer-use plugin: `.zcode-plugin/plugin.json` defines a stdio server, hosted by ZCode Helper as `zcode.cjs __zcode-plugin-host <server.js>`); the registered server name is `plugin:<plugin>:<server>`. [S31][S33]
- **Tool naming**: `mcp__<server>__<tool>` (confirmed by local logs: `mcp__langchain_docs__query_docs_filesystem_docs_by_lang_chain`). [S34]
- **Runtime behavior** (log-confirmed): startup events `mcp.startup.completed` (serverCount/statusCounts/toolCount) and `mcp.tools.registered` (registeredToolCount); connections are pooled via `adapters.mcp.pool`, with `mcpIsolation: "session"` (isolated per session); plugin servers configure `timeoutMs: 90000`. [S34]
- **OAuth**: http/sse remote servers only; clicking "Open authorization" goes through the system browser, and the docs stress that "ZCode never opens a browser window on its own". [S32]
- **Misc**: supports one-click import from Claude Code / Codex / OpenCode / generic `.agents/mcp.json`; permissions go through `tool.permission.resolved` (modes include yolo among others); no documentation or trace of "itself as an MCP server". [S32][S34]

## 7. pi and pi-core-rs

### 7.1 TS side (badlogic/pi-mono → earendil-works/pi, ~103k stars)

- A case-insensitive tree-wide search for `mcp` yields **0 hits**: there is no `@mariozechner/pi-mcp`-style package, and none exists on npm. pi extends its tools through the extension system rather than building in MCP. [S35]
- Author Mario Zechner laid out his design position in the blog post "What if you don't need MCP at all?": general-purpose MCP servers pile up tools to "cover every scenario" with verbose descriptions (measured: Playwright MCP 13.7k tokens ≈ 6.8% of context, Chrome DevTools MCP 18k ≈ 9%) and poor extensibility/composability; he prefers the progressive-disclosure route of small CLI scripts + skills, while conceding the cost of "shouldering tool maintenance yourself". [S36]

- Re-verified 2026-09-21: pi (TS) and pi-core-rs still ship no built-in MCP; but the ecosystem has produced the third-party extension `pi-mcp-adapter` (lazy connection + a single proxy tool to save context; natively reads `.mcp.json` and `~/.config/mcp/mcp.json`; host-config import goes through the explicit `/mcp setup`, with auto-discovery off by default) — the official "no MCP" line is unchanged, and MCP is handled by the extension layer. [I14]

### 7.2 Rust side (pi-core-rs, Holt's fork source)

- `Onion-L/pi-core-rs` ("A standalone Rust runtime for Pi's agent, AI, and telemetry core") has **no mcp-named files** across its 329-file tree; the crate layers are `agent`/`ai`/`bin`/`telemetry`. [S37]
- The tool abstraction is the plain-data-plus-closure `AgentTool`: `{ name, label, description, parameters: serde_json::Value (JSON Schema), execute: Arc<dyn Fn(&str, &Value, ...) -> BoxFuture<Result<AgentToolResult, String>>> }`; the harness's built-in bash/read/write/edit are implemented against exactly this structure. **There is no MCP client at all**. [S37]

### 7.3 What this means for Holt

pi-core-rs not providing MCP means the integration point lies entirely on Holt's side; and `AgentTool` happens to be exactly the shape other projects reduce an MCP tool to (name/description/JSON schema/execute closure), see §10.

## 8. Other projects at a glance

### 8.1 google-gemini/gemini-cli (TS, ~106.9k stars)

Top-level `mcpServers` in `settings.json` (one of `command`|`url` (SSE)|`httpUrl`, plus `headers`/`env`/`cwd`/`timeout` defaulting to 10 minutes/`trust` to skip confirmation/`includeTools`/`excludeTools`) plus the global `mcp.allowed/excluded`. Built on `@modelcontextprotocol/sdk`; tools are registered under the FQN **`mcp_<server>_<tool>`** (single underscore; the docs warn against underscores in server names, or permission rules parse out of alignment); `/mcp` shows tools/prompts/resources and `/mcp auth` manages OAuth (a 401 triggers auto-discovery + browser flow); schemas are sanitized for Gemini API compatibility (stripping `$schema`, `additionalProperties`, etc.). [S38]

### 8.2 QwenLM/qwen-code (TS, ~27.7k stars)

A gemini-cli fork; `packages/core/src/tools/` contains the same full `mcp-client.ts`/`mcp-tool.ts` set (which has since evolved fork-specific files such as a transport pool and session views), and the `mcpServers` configuration mechanism shares its origin with gemini-cli. [S39]

### 8.3 cline/cline (VS Code extension, ~67.7k stars)

Depends on `@modelcontextprotocol/sdk@^1.25.1`; `apps/vscode/src/services/mcp/McpHub.ts` centrally manages connections, with the transport trio `StdioClientTransport`/`SSEClientTransport`/`StreamableHTTPClientTransport`. Configuration lives in the globally stored `cline_mcp_settings.json` (constant `CLINE_MCP_SETTINGS_FILE_NAME`, hot-reloaded via chokidar watch); per-tool auto-approve (the `toggleToolAutoApprove` controller); ships an MCP marketplace, remote MCP, and `McpOAuthManager` OAuth; the CLI edition provides `cline mcp`. [S40]

### 8.4 RooCodeInc/Roo-Code (TS, ~24.3k stars)

A Cline fork; the same `McpHub.ts` and three transports; the differences are the config schema's built-in `alwaysAllow: z.array(z.string())` **per-tool permanent-allow list**, and a separate `McpServerManager` shared across windows. [S41]

### 8.5 block/goose (Rust, ~54.1k stars, now aaif-goose/goose)

MCP-first: "extensions" are MCP servers; built-in extensions such as Developer/Memory/Computer Controller are enabled by default, and goose automatically screens for malicious packages before installing an extension. The Rust side uses **rmcp** (`transport-child-process`, `transport-streamable-http-client(-reqwest)`, `transport-streamable-http-client-unix-socket`) + `oauth2`. `goose configure` → Add Extension, or `goose mcp {name}` (note: this runs one of goose's **built-in** MCP servers — the source enum contains only AutoVisualiser/ComputerController/Memory/Tutorial — not goose serving itself wholesale as a server); when acting as an agent to the outside it speaks ACP (`goose acp`, stdio). Permission modes: Completely Autonomous (**default**) / Manual Approval / Smart Approval (auto-approves low-risk) / Chat Only, plus per-tool permissions. [S42]

### 8.6 sst/opencode (TS, ~206.1k stars, now anomalyco/opencode)

Top-level `mcp` in `opencode.json`, either `type: "local"` (`command` is an **array**, `environment`, `timeout` defaulting to 5000ms) or `"remote"` (`url`, `headers`, and `oauth`, which can be set to false to disable auto-detection); OAuth uses DCR with the default callback `http://127.0.0.1:19876/mcp/oauth/callback`. Early versions used the Vercel AI SDK's `experimental_createMCPClient`; the current source imports the three transports from `@modelcontextprotocol/sdk` directly. [S43]

### 8.7 charmbracelet/crush (Go, ~28k stars)

The README advertises three transports, stdio/http/sse; configuration is the `mcp` key in `crush.json` or the `mcp add` command (`--command/--args/--timeout/--disabled-tools/--header`), supporting the `oauth: true` authorization-code flow and pre-registered clients. The source uses the **official Go SDK** `github.com/modelcontextprotocol/go-sdk/mcp` (not the earlier third-party mcp-go); `internal/agent/tools/mcp/` handles lifecycle reconcile and tool injection, and implements the channel capability (servers can push events into the session). [S44] Note 2026-09-21: crush's config system has been rewritten as the Bash-style `crushrc` (`mcp add` subcommand); the JSON `mcp` key is marked deprecated and receives no new features, see §10.3. [I12]

### 8.8 All-Hands-AI/OpenHands (~87.1k stars)

The project has been rewritten as a TypeScript/Electron desktop app; MCP servers are managed in the settings UI (`src/routes/mcp-settings.tsx`; specs/mcp-settings.md specifies persistence semantics such as sparse changes and secret retention). Historically, the Python version configured sse/http remote and stdio local servers in the `[mcp]` section of `config.toml` and converted MCP tools into agent actions — that path evolved with the rewrite; see the official docs for details. [S45]

### 8.9 Aider-AI/aider (Python, ~48.9k stars)

**The counter-example that explicitly refuses MCP**. The README has no MCP integration; the feature request issues #2525 ("Please add support for model context protocol") and #3314 ("MCP SUPPORT") have stayed open for a long time, and community PRs (#3672, #3937) were never merged; users bridge via external tools like mcpm-aider and AiderDesk. Its default toolset (shell/file editing/built-in web) is self-sufficient, and the author has not accepted the MCP route. [S46]

### 8.10 continuedev/continue (TS, ~35.8k stars)

A top-level `mcpServers` list in `config.yaml`, or standalone blocks in workspace `.continue/mcpServers/*.yaml` (requiring `name`/`version`/`schema` metadata); **directly accepts Claude Desktop/Cursor/Cline JSON `mcpServers` files** dropped into that directory. Type supports `stdio`/`sse`/`streamable-http`. MCP takes effect only in agent mode. [S47]

### 8.11 aws/amazon-q-developer-cli (Rust, ~2k stars)

`crates/agent/src/agent/mcp` implements the client with **rmcp** (`rmcp::Service<RoleClient>`); the agent config JSON embeds an `mcpServers` field (verbatim source comment: "This is to mirror claude's config set up"), and it also supports a standalone `mcp.json` (top-level `mcpServers`) and OAuth (`mcp_client/oauth_util.rs`). Tool admission relies on the agent's **`allowedTools`** set as an allowlist (combined with server names to identify MCP tool references). [S48]

### 8.12 zed-industries/zed (Rust, open-source)

> Consulted 2026-09-21, source anchored at `main@85b8198e`, zed.dev docs from the same day; citations [Z*] are listed in §12.

- **Config**: the `context_servers` key (project settings namespace; user `settings.json` and project `.zed/settings.json` share the schema), `#[serde(untagged)]` into three shapes: stdio / http / extension (the extension supplies the launch command). [Z2]

```jsonc
{
  "context_servers": {
    "local-server": {
      "enabled": true,                        // default true
      "command": "npx",
      "args": ["-y", "some-mcp-server"],
      "env": { "KEY": "value" },              // merged incrementally into the inherited process environment
      "timeout": 120,                         // seconds; tool-call timeout, overrides the global
      "remote": false                         // default false; true runs on the remote side in remote development
    },
    "remote-server": {
      "enabled": true,
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" },
      "oauth": { "client_id": "...", "client_secret": "..." }  // pre-registered credentials (when the server lacks DCR)
    },
    "extension-server": { "enabled": true, "settings": { } }   // free-form JSON defined by the extension
  },
  "context_server_timeout": 60               // global default call timeout (seconds)
}
```

- Timeout chain: per-server `timeout` > global `context_server_timeout` (default 60s), clamped at a 600s ceiling. [Z2][Z3] The UI entry is Settings → AI → MCP Servers (`Add Local/Remote Server`, `Install from Extensions`), writing the same schema. [Z19] Extension-shaped MCP servers are slated for deprecation in favor of the official MCP registry (issue #59351). [Z18]
- **Reads no `.mcp.json`, and has no mechanism whatsoever for importing other tools' configs** (zero hits in a repo-wide grep; the official docs are explicit that external agents read their own native MCP config, and Zed only forwards the servers it configured itself). [Z17]
- **Hand-rolled implementation**: `crates/context_server` hand-writes JSON-RPC 2.0 + MCP types with no dependency on rmcp / the official SDK; it negotiates the four protocol versions from 2024-11-05 to 2025-11-25. [Z4][Z6][Z8] Transports are stdio + streamable HTTP only (single-endpoint POST, SSE response stream, `Mcp-Session-Id` sessions, `DELETE` on teardown, `MCP-Protocol-Version` header attached for ≥2025-06-18), with no legacy SSE GET. [Z5]
- **Complete OAuth**: 401 → RFC 9728 resource metadata → discovery → DCR (CIMD supported; settings can pre-register a `client_id`) → loopback callback + PKCE → tokens stored in the system keychain and auto-refreshed; a 401 after initialize is likewise caught by the transport-shutdown listener and routed back into re-authorization. [Z9]
- **Tool exposure**: once a server reaches `Running`, `tools/list` runs and each MCP tool is wrapped as a `ContextServerTool`. The LLM sees **bare tool names** (sanitized to `[A-Za-z0-9_-]`, ≤64 characters), with a `<server_snake>_<tool>` prefix added only on cross-server or built-in-tool name collisions; settings/approvals use the stable ID `mcp:<server>:<tool>`. Profile-level switch: `agent.profiles.<id>.enable_all_context_servers` (by default all-on in write, all-off in ask). [Z10][Z11][Z15]
- **Approval**: default is `confirm` (controlled by `agent.tool_permissions.default` since v0.224.0); MCP tools support only per-tool `default: allow|deny|confirm`, no input-pattern regexes; the agent panel's dropdown approval card ("Always for X MCP tool" persists to settings). [Z12][Z13][Z14][Z20]
- **Lifecycle**: all enabled servers start eagerly with Project creation; settings/extension/worktree changes trigger a reconcile (config change → restart, deletion → removal, `enabled:false` → stopped but retained); on failure it enters Error with **no automatic retry** — restart is manual via the UI; the state machine is Starting/Running/Stopped/Error/AuthRequired/ClientSecretRequired/Authenticating, with status dots in Settings → AI → MCP Servers; a stdio process dying for non-auth reasons does not change the state (a pitfall the source comments admit to). [Z3][Z19]
- `tools/list_changed` is subscribed (full re-fetch, effective the next turn), but **`tools/list` does not follow `next_cursor` pagination** — tools beyond the first page are lost, a known weakness. [Z10][Z11]
- **Reverse: does not expose itself as an MCP server** (the `context_server` crate is client-side only); external integration goes through ACP: at `newSession` it forwards its configured MCP servers to external agents, and the Zed Agent itself can also run as an ACP agent server. [Z4][Z16][Z21][Z22]

## 9. Cross-comparison

| Project | Language | SDK | Config | Transport | Tool naming | Permission granularity | OAuth | Lazy loading | Reverse MCP server |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Codex CLI | Rust | rmcp (official) | `config.toml` `mcp_servers` | stdio, Streamable HTTP (SSE deleted) | bare `tool.name` | server approval mode + per-tool approval/output caps | DCR/CIMD/chatgpt, `mcp login` | catalog cache + pagination cap + startup grace | deprecated `mcp-server` → app-server |
| Claude Code | closed-source | MCP TS SDK 1.x/2.0 | `claude mcp add` + `.mcp.json`, 3 scopes + plugin/managed | stdio, http, sse (deprecated), ws | `mcp__server__tool` | server/tool regex rules + requiresUserInteraction | `/mcp` + `mcp login`, auto-refresh | Tool Search on by default, `auto:N` threshold, `alwaysLoad` | `claude mcp serve` |
| Kimi CLI | Python | fastmcp (wrapping official mcp) | `~/.kimi/mcp.json` + `kimi mcp add` | stdio, Streamable HTTP | bare `tool.name` (server in description) | every call prompts; YOLO/AFK auto-allow | `--auth oauth`, `kimi mcp auth` | async load after UI, deferrable | none |
| ZCode | closed-source | undisclosed | `~/.zcode/cli/config.json` / `.zcode/config.json` `mcp.servers` + `.agents` fallback + plugins | stdio, http, sse | `mcp__server__tool` (log-confirmed) | auto-connect + Settings audit; mode permissions | browser authorization button | per-session connection pool, startup toolCount summary | none (pure host) |
| pi / pi-core-rs | TS/Rust | none | none | none | — | — | — | — | none |
| Zed | Rust | hand-rolled (`crates/context_server`) | settings.json `context_servers` + Settings UI | stdio, Streamable HTTP | bare names (prefix on collision); permission ID `mcp:<server>:<tool>` | per-tool allow/deny/confirm | PKCE+DCR/CIMD, keychain | none (no pagination, full injection) | none (ACP outward) |
| gemini-cli | TS | @modelcontextprotocol/sdk | `settings.json` `mcpServers` | stdio, SSE, Streamable HTTP | `mcp_server_tool` FQN | `trust` + include/excludeTools + Policy Engine | auto-discovery, `/mcp auth` | none (full injection) | none |
| qwen-code | TS | same as gemini | same as gemini (fork) | same as gemini | same as gemini | same as gemini | same as gemini | none | none |
| cline | TS | @modelcontextprotocol/sdk | `cline_mcp_settings.json` + marketplace | stdio, SSE, Streamable HTTP | server name + tool name | per-tool auto-approve | McpOAuthManager | none | none |
| Roo-Code | TS | @modelcontextprotocol/sdk | same as cline (fork) | same as cline | same as cline | per-tool `alwaysAllow` | same as cline | none | none |
| goose | Rust | rmcp (official) | `goose configure` + config | stdio, Streamable HTTP, unix socket | extension:tool | four modes (fully automatic by default) + per-tool | oauth2 crate | Code Mode (dynamic discovery in JS) | built-in extensions can serve (`goose mcp <name>`) |
| opencode | TS | @modelcontextprotocol/sdk (migrated from AI SDK) | `opencode.json` `mcp` (local/remote) | stdio, SSE, Streamable HTTP | server name + tool name | enabled switch + approval system | DCR, callback port 19876 | none | none |
| crush | Go | modelcontextprotocol/go-sdk (official) | `crush.json` `mcp` + `mcp add` | stdio, http, sse | server name + tool name | `disabledTools` + approval | `oauth: true` + pre-registration | none | none |
| OpenHands | TS (post-rewrite) | not verified item by item | settings UI (formerly config.toml `[mcp]`) | sse, http, stdio (historical) | server + tool | managed in the settings UI | yes (historical docs) | none | none |
| aider | Python | none | none | none | — | — | — | — | — |
| continue | TS | official SDK | `config.yaml` / `.continue/mcpServers/*.yaml` + compatible JSON | stdio, sse, streamable-http | server name + tool name | agent-mode permissions | — | none | none |
| Amazon Q CLI | Rust | rmcp (official) | agent JSON `mcpServers` + `mcp.json` ("mirror claude") | stdio, http | server + tool | `allowedTools` allowlist | oauth_util | none | none |

## 10. Config interop: importing other tools' MCP configs (amended 2026-09-21)

> This section answers "how to carry MCP servers already configured in other tools into Holt". The verification baseline is this document's 2026-09-10 snapshot; citations [I*] are listed in §12.

### 10.1 Claude Code: schema re-check (snapshot accurate but incomplete)

- Storage: local is `projects["<abs-path>"].mcpServers` in `~/.claude.json`; project is `.mcp.json` at the repo root; user is top-level `mcpServers` in `~/.claude.json`. `~/.claude/settings.json` has **no** `mcpServers` key — ZCode's docs claim to import Claude config from `~/.claude/settings.json`, which is the wrong path; an importer should trust `~/.claude.json` + `.mcp.json`. [I1][I2][I6]
- Entry fields: stdio takes `command` (the discriminator)/`args`/`env`, with `type:"stdio"` optional; http requires `type:"http"` (`streamable-http` is an alias) + `url`, plus optional `headers`, `headersHelper` (a shell command generating headers dynamically), `timeout` (ms, per-call), `alwaysLoad`, `oauth{clientId,callbackPort}` (camelCase); sse is the same as http (deprecated); ws is JSON-config-only. **Entries with a `url` but no `type` are skipped wholesale**. [I1]
- `${VAR}`/`${VAR:-default}`: applies to command/args/env/url/headers; unset with no default → the literal is kept + a warning; credential-type variables in remote url/headers (`ANTHROPIC_API_KEY`, `HTTPS_PROXY`, etc.) read as empty strings and `:-default` is ignored (preventing host credentials from leaking through config files); the display surface always shows the unexpanded reference. [I1]
- Server names are restricted to `[A-Za-z0-9_-]`, and reserved names are skipped at load; Claude Code's own import capabilities are only `add-from-claude-desktop` (interactive checkbox selection) + manual conversion via `add-json`. [I1]

### 10.2 Codex: field-set changes (snapshot partly stale)

- **Default timeouts 10s/60s → 30s/300s** (`DEFAULT_STARTUP_TIMEOUT` / `DEFAULT_TOOL_TIMEOUT`). [I5]
- RawMcpServerConfig additions/changes: the `auth` enum gained `ema_auth` (enterprise IdP); `oauth` gained `authorization_server_issuer`; new fields `startup_timeout_ms` (sec takes precedence), `environment_id` (default `"local"`), `omit_tools_from` (hides tools from per-model visibility surfaces), `name` (legacy compat), and `bearer_token` (explicitly accepted but always errors, to catch misconfiguration); `env_vars` upgraded to the structured `{name, source: local|remote}`; new global keys `mcp_enterprise_managed_auth` and `[orchestrator] mcp.enabled`. The transport enum remains Stdio + StreamableHttp only (snapshot correct). [I3][I4]
- Little impact on interop (the new fields are all ignorable), but places citing the old defaults need updating.

### 10.3 How each project handles "importing someone else's config" (three tiers)

- **Active scanning + checkbox import**: ZCode (Settings → MCP Servers → Import scans the four sources Claude Code / Codex / OpenCode / `.agents/mcp.json`, writes native config, leaves source files untouched, and defaults imported servers to enabled) [I6]; pi-mcp-adapter (`/mcp setup` shows → select → preview → write; host-config auto-discovery defaults to `"off"`; never writes back to external files, never silently executes the commands inside them) [I14].
- **Direct file-level reads**: Continue auto-loads `mcpServers` JSON from Claude Desktop / Cursor / Cline copied into `.continue/mcpServers/` [I8]; ZCode / pi-mcp-adapter read `.agents/mcp.json` as a fallback; kimi's `--mcp-config-file <path>` can load mcpServers JSON from any location (its remote entries with only `url` and no `type` still parse as http — the opposite of Claude) [I10].
- **Export-only / no import**: opencode (organizations can push default remote servers via `.well-known/opencode`) [I9], crush, gemini-cli, Claude Code (accepts Claude Desktop only). Nobody does "silently read someone else's entire config" — explicit and user-visible is the mainstream security posture.
- Aside: crush's config system has been rewritten as the Bash-style `crushrc` (`mcp add <name> --type stdio|sse|http --command/--args/--env/--url/--header/--timeout/--oauth…`; the §8.7 snapshot is stale) [I12]; gemini additionally offers the standard-shaped `.gemini/mcp_config.json` entry point, and its stdio subprocesses by default scrub credential-type variables from the host environment (`*TOKEN*/*SECRET*/*PASSWORD*/*KEY*/*AUTH*` patterns; explicitly writing them into env counts as informed consent) — the scrubbing is worth copying. [I11]

### 10.4 Is `.agents/mcp.json` a standard?

- **There is no governing spec**; it is a de facto convention: ZCode treats it as a same-scope fallback (if the same scope's `.zcode` has any server, the whole file is ignored — no merging; unknown keys discard the whole server) [I6][I7], and pi-mcp-adapter lists `~/.agents/mcp.json` as a tool-agnostic path [I14]. A cheap-compat bonus, but do not implement it as though it were a spec.
- The one with actual spec backing is **Agent Plugins 1.0.0** (agent-plugins.org, co-governed by Amazon/Cursor/Microsoft/OpenAI/Vercel): a plugin-root `mcp.json` with a closed schema (top level holds only `$schema` + `mcpServers`), transports stdio (`command/args/env/cwd`) / streamable-http (`url/headers` as literals, credentials forbidden) / sse (deprecated, optional), placeholders limited to `${PLUGIN_ROOT}`/`${PLUGIN_DATA}`, and no OAuth defined in 1.0.0 — it governs plugin packaging, not user config files. [I13]
- `~/.config/mcp/mcp.json`: the XDG-style user-global path pi-mcp-adapter primarily promotes; currently a single adopter. [I14]

### 10.5 The minimal interop field set (what Holt must parse to accept others' configs)

Must parse/normalize:

| Field | Appears in | Holt import handling |
| --- | --- | --- |
| server name | all (map key) | sanitize to `[A-Za-z0-9_-]`, add a suffix on collision |
| `command` | all stdio (**opencode uses an array**) | array split: first element → command, the rest prepended as args |
| `args` / `cwd` | all / gemini·codex·pi·Agent Plugins | pass through / default to the project root |
| `env` | all (opencode calls it `environment`) | rename to normalize; expand variables inside values |
| `url` + `type` | all remote | normalize `local`→stdio, `remote`/`streamable-http`→http, `sse`→compat or reject, `ws`→warn and ignore; also accept kimi's "url means http" |
| `headers` | all (codex calls them `http_headers`) | rename to normalize; expand inside values |
| `enabled` | codex/opencode/pi (zcode accepts both spellings) | normalize into a switch |
| `includeTools`/`enabled_tools`, `excludeTools`/`disabled_tools` | gemini·pi / codex·crush | normalize into allow/blocklists (deny wins) |
| `timeout` | everywhere (**units and semantics differ across the board**: claude per-call ms, gemini request ms default 600000, opencode ms default 5000, zcode `timeoutMs` default 30000, codex seconds 30/300, crush startup seconds) | convert per source semantics + clamp, fall back to Holt defaults, never blindly copy values |
| `${VAR}`/`${VAR:-default}` | claude/zcode (plugins)/pi | **must implement**; unset with no default → keep the literal + warn (Claude semantics) |
| `trust` | gemini | map to an "approval-free" hint, default false |

Ignorable without error (list each in the import log): Claude's `headersHelper` (**its value is an arbitrary shell command — discard + loud warning**), `alwaysLoad`, `oauth`, ws entries; Codex's `auth`/`oauth*`/`scopes`/`required`/`supports_parallel_tool_calls`/`omit_tools_from`/`default_tools_approval_mode`/`tools{}`/`env_vars`/`environment_id`/`name`, and `bearer_token` (the env var name may be kept, never inline the value); gemini's Google-proprietary `authProviderType`/`targetAudience`/`oauth{…}`; generic unknown keys are leniently ignored across the board — ZCode's strict discard is the cautionary tale (users silently lose servers). [I3][I7]

Quick difference reference (importer test matrix):

| Dimension | Claude Code | Codex | opencode | kimi | gemini-cli | crush | ZCode | Continue |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Wrapper key | `mcpServers` | TOML `mcp_servers` | `mcp` | `mcpServers` | `mcpServers` | Bash `mcp add`/legacy `mcp` | `mcp.servers`/`mcpServers` | list YAML + direct JSON read |
| command shape | string | string | **array** | string | string | string | string | string |
| env / headers key names | `env`/`headers` | `env`/`http_headers` | `environment`/`headers` | `env`/`headers` | `env`/`headers` | `env`/`--header` | `env`/`headers` | yaml |
| remote detection | `type`+`url` (skipped without type) | has `url` | `type:"remote"` | **url means http** | `url`=SSE / `httpUrl`=HTTP | `--type` | `type`/inferred | `type` |
| env expansion | `${VAR}`, `${VAR:-d}` | none | `{env:VAR}` | none | `$VAR`/`${VAR}`/`%VAR%` (env only) | shell expansion | plugins only | `${{ secrets.X }}` |
| tool filtering | none (permissions handled separately) | enabled/disabled_tools | none | none | include/excludeTools | enabled/disabled-tools | none | none |
| timeout semantics | per-call ms | startup/call seconds | per-tool ms | set elsewhere | request ms | startup seconds | timeoutMs | none |

### 10.6 Summary of stale snapshot items

| Old entry | Current state (2026-09-21) | Source |
| --- | --- | --- |
| §3.2 Codex defaults 10s/60s | 30s/300s | [I5] |
| §3.2 Codex field set | + EMA/auth three values/`startup_timeout_ms`/`omit_tools_from`/`environment_id` etc. | [I3][I4] |
| §8.7 crush config in the `mcp` key of `crush.json` | the Bash `crushrc` is canonical; JSON deprecated and receiving no new features | [I12] |
| §7.1 pi ecosystem has no MCP | the third-party `pi-mcp-adapter` has appeared (lazy + proxy-tool route), not core; pi itself still has none | [I14] |
| §7.2 pi-core-rs has no MCP | still holds (re-verified in the local cargo checkout; the only grep hit is an OAuth scope string) | [I15] |
| Item 3 of the "Implications" recommending the `mcp.servers` nested shape | collides with ZCode's native shape in name but not semantics; if you also accept the `.agents/mcp.json` fallback, avoid ZCode's "no same-scope merging" pitfall | [I6][I7] |

### 10.7 Recommended import strategy

1. **An explicit importer, no silent environment scanning**: UI entry → scan candidate sources → checkbox list (with source/transport/full command+env preview) → write into Holt's native config, **never writing back to the source files**. Model after ZCode [I6] and pi-mcp-adapter (host discovery off by default) [I14].
2. **Candidate sources (phase 1)**: project `.mcp.json`; `~/.claude.json` (top level + `projects[*].mcpServers`, not settings.json); `[mcp_servers.*]` in `~/.codex/config.toml`; `~/.agents/mcp.json` and `~/.config/mcp/mcp.json`; `~/.kimi/mcp.json`; `.continue/mcpServers/*.json`. opencode/crush/gemini diverge more in shape and pay off less per machine — phase 2. [I1][I3][I6][I8][I10][I14]
3. **The parsing layer** normalizes per §10.5; unknown fields are leniently ignored and surfaced in an import report; fields whose values are shell commands (`headersHelper`/`http_headers_helper`) are discarded + warned about; no credential is ever inlined.
4. **The expansion layer**: implement `${VAR}`/`${VAR:-default}` (Claude semantics); handle gemini's `$VAR` and opencode's `{env:VAR}` during conversion; Holt's native config stores the pre-expansion form, and expansion happens at runtime.
5. **Trust boundary**: importing a project-level file counts as confirmation (the user has already seen the full command in the dialog); organization/remote-pushed sources do not carry over on import.
6. **Convert timeouts by semantics and clamp** — do not adopt gemini's 600000ms or codex's 30s/300s as Holt defaults.
7. **Phase 2**: Agent Plugins `mcp.json` as the plugin/distribution format (closed schema, validatable against its JSON Schema) [I13]; `.agents/mcp.json` as an optional read fallback, but implement merging or explicit precedence — do not copy ZCode's no-merging. [I6][I7]

## 11. Implications for Holt

1. **Integration point: the engine layer, producing `AgentTool`s.** pi-core-rs's `AgentTool` (name/description/JSON schema/execute closure) is precisely the unified shape every project reduces MCP tools to; build an MCP client manager inside `crates/engine` (candidate: the official `rmcp`, the same one Codex/Goose/Amazon Q use), wrap each `tools/list` result as an `AgentTool`, and forward `tools/call` inside `execute`. The UI need not know MCP exists and keeps rendering only `MessagePart`s — consistent with ARCHITECTURE's UI/RPC layering. Note the RPC contract needs a matching extension (server list/status/approval events), or the UI cannot render "an MCP call awaiting approval".
2. **Naming: prefix from day one.** Codex's and Kimi's bare-name schemes are simple but make collision handling, permission rules, and readability all harder; Claude Code's `mcp__server__tool` has become industry consensus (ZCode follows it, and Claude-family tools stay mutually compatible), and it lets permission rules express both the `mcp__server` / `mcp__server__tool` tiers.
3. **Config: stay compatible with the `mcpServers` shape.** Claude's JSON shape is the de facto standard (ZCode's `.agents/mcp.json` fallback, Continue's direct JSON read, and Amazon Q's mirror comment are all evidence). Holt's data directory is `~/.holt`; suggest `mcp.servers` inside `~/.holt/config` (nested, with `enabled`/`timeoutMs`/`headers`), and keep file parsing strict (error on unknown keys rather than ignoring them — ZCode's silent discard has burned many users). For the importer's field normalization, the per-project difference reference, and the import strategy, see §10.
4. **Transport: start with stdio + Streamable HTTP and skip SSE outright.** The spec has completed its transition to Streamable HTTP, and of the two leading implementations one has deleted SSE and the other marked it deprecated; rmcp's `transport-child-process` + `transport-streamable-http-client` features cover it, no hand-written transport needed.
5. **Approval and trust: confirm before connecting project-level servers.** Claude Code (first-use prompt for project scope) and ZCode (auto-connect + Settings audit) mark the two extremes; the middle ground is the sensible one — auto-connect user scope, require first-use confirmation for workspace scope — while remembering Claude's lesson: headless sessions like `claude -p` must not get stuck on confirmation.
6. **Do resource control early: timeouts, output caps, catalog caps.** Codex (tool_timeout 60s, 2048 tools/100 pages of pagination per server, per-tool output_token_limit) and Claude (25k-token output cap, 2KB description truncation, automatic backgrounding on timeout) have both hardened "flood prevention" into real mechanisms, in line with the core of the pi author's critique of MCP context cost [S36]. Holt's RPC contract already carries protective comments about MCP payloads (crates/engine/src/agent.rs); keep building the caps into the contract along that line.
7. **Lazy loading can wait, but reserve the interface.** Claude's Tool Search depends on model-side `tool_reference` and cannot be replicated short-term; the pragmatic path is Codex-style catalog caching + `enabled_tools/disabled_tools` allowlists, plus injecting server instructions into the system prompt (listing names on demand when tools are many). Handle `listChanged` notifications and `tools/list` pagination from day one, to avoid reworking the protocol layer later.
8. **OAuth can ship in two phases.** Phase 1: stdio + static `headers`/`bearer_token_env_var` (covers most local servers); phase 2: the authorization-code flow with RFC 8707 + protected resource metadata (see Codex's `codex mcp login` and opencode's DCR implementation; the callback port must be pinnable to stay compatible with pre-registered URIs).
9. **A reverse server is not a must-have.** Claude Code / Codex offer that mode mainly to serve "embedding the agent into other harnesses"; Holt's equivalent shape is closer to exposing through the existing `RpcService` — skip it until there is external-consumer demand.
10. **Stay usable without MCP.** pi's stance is worth borrowing: built-in tools (shell/edit/retrieval) + skills carry 80% of the work, with MCP as an extension channel rather than a core dependency — aider's outright refusal and Zed-style full adoption each carry their own costs.

## 12. Primary-source index

### MCP specification

- [S1]: https://modelcontextprotocol.io/specification/latest (architecture and roles; latest is 2026-07-28)
- [S2]: https://modelcontextprotocol.io/specification/latest (extensions: Tasks / MCP Apps / Skills over MCP)
- [S3]: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports (stdio, Streamable HTTP, HTTP+SSE compatibility, session/version headers)
- [S4]: https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- [S5]: https://modelcontextprotocol.io/specification/2025-06-18/changelog (OAuth resource server, RFC 8707, elicitation, etc.)
- [S6]: https://github.com/openai/codex (main repo)
- [S7]: https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/Cargo.toml
- [S8]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (RawMcpServerConfig / AppToolApproval / McpServerAuth / transport enums)
- [S9]: https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs (mcp_servers, oauth global keys, grace period)
- [S10]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (McpServerTransportConfig: Stdio/StreamableHttp)
- [S11]: https://github.com/openai/codex/blob/main/codex-rs/tools/src/mcp_tool.rs (bare names, properties backfill, 1000B truncation); https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/catalog.rs (same-name collisions)
- [S12]: https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/pagination.rs (100 pages/2048 items)
- [S13]: https://github.com/openai/codex/blob/main/codex-rs/cli/src/mcp_cmd.rs (list/get/add/remove/login/logout)
- [S14]: https://learn.chatgpt.com/docs/mcp-server (codex mcp-server deprecation notice; codex/codex-reply tools)
- [S15]: https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs (Subcommand: no mcp-server, has app-server)

### Claude Code

- [S16]: https://code.claude.com/docs/en/mcp (setup/transports/timeout tips/resources/roots)
- [S17]: https://code.claude.com/docs/en/mcp (scopes, precedence, ${VAR} expansion)
- [S18]: https://code.claude.com/docs/en/mcp (plugin MCP, `mcp__plugin_*` naming)
- [S19]: https://code.claude.com/docs/en/permissions (mcp__ rules and glob restrictions)
- [S20]: https://code.claude.com/docs/en/mcp (anthropic/requiresUserInteraction)
- [S21]: https://code.claude.com/docs/en/mcp (Tool Search, ENABLE_TOOL_SEARCH, alwaysLoad)
- [S22]: https://code.claude.com/docs/en/mcp (MCP_TIMEOUT/MCP_TOOL_TIMEOUT/idle timeout/automatic backgrounding)
- [S23]: https://code.claude.com/docs/en/mcp (output caps, maxResultSizeChars)
- [S24]: https://code.claude.com/docs/en/mcp (v1/v2 runtime, list_changed, reconnection)
- [S25]: https://code.claude.com/docs/en/mcp (prompts→commands, resources→@, elicitation)
- [S26]: https://code.claude.com/docs/en/mcp (claude mcp serve)
- [S27]: https://code.claude.com/docs/en/agent-sdk/mcp (SDK mcpServers/allowedTools/in-process SDK servers)

### Kimi CLI

- [S28]: https://github.com/MoonshotAI/kimi-cli
- [S29]: https://github.com/MoonshotAI/kimi-cli/blob/main/src/kimi_cli/soul/toolset.py (MCPTool bare names/approval/timeout); https://github.com/MoonshotAI/kimi-cli/blob/main/pyproject.toml (fastmcp==3.2.4)
- [S30]: https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/mcp.md (kimi mcp subcommands, ~/.kimi/mcp.json, OAuth storage)

### ZCode

- [S31]: local file `/Users/onion/.zcode/cli/plugins/cache/zcode-plugins-official/zcode-guide/0.1.0/skills/diagnosing-mcp/SKILL.md` (first-hand: config locations/override order/schema/timeouts/strict validation)
- [S32]: https://zcode.z.ai/en/docs/mcp-services (official docs: transport, OAuth, import, trust)
- [S33]: local file `/Users/onion/.zcode/cli/plugins/cache/zcode-plugins-official/computer-use/0.5.13/.zcode-plugin/plugin.json` (a plugin mcpServers instance)
- [S34]: local files `/Users/onion/.zcode/cli/log/zcode-2026-09-09.jsonl`, `zcode-2026-09-10.jsonl` (mcp.startup.completed, mcp.tools.registered, mcp.pool.*, mcpIsolation, `mcp__langchain_docs__*` naming instances)

### pi / pi-core-rs

- [S35]: https://github.com/earendil-works/pi (formerly badlogic/pi-mono; no mcp files in tree)
- [S36]: https://mariozechner.at/posts/2025-11-02-what-if-you-dont-need-mcp/ (author's position and token-cost data)
- [S37]: https://github.com/Onion-L/pi-core-rs (README layering; the AgentTool definition in src/agent/types.rs; no MCP module)

### Other projects

- [S38]: https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md
- [S39]: https://github.com/QwenLM/qwen-code (packages/core/src/tools/mcp-*.ts)
- [S40]: https://github.com/cline/cline (apps/vscode/src/services/mcp/McpHub.ts, constants.ts, apps/vscode/package.json)
- [S41]: https://github.com/RooCodeInc/Roo-Code/blob/main/src/services/mcp/McpHub.ts (alwaysAllow schema)
- [S42]: https://github.com/aaif-goose/goose (documentation/docs/getting-started/using-extensions.md, guides/managing-tools/goose-permissions.md, crates/goose/Cargo.toml, crates/goose-cli/src/cli.rs, crates/goose-mcp/src/mcp_server_runner.rs)
- [S43]: https://github.com/anomalyco/opencode (packages/opencode/src/mcp/index.ts, packages/core/src/v1/config/mcp.ts)
- [S44]: https://github.com/charmbracelet/crush (README.md MCP section, go.mod, internal/agent/tools/mcp/channel.go)
- [S45]: https://github.com/All-Hands-AI/OpenHands (specs/mcp-settings.md, src/routes/mcp-settings.tsx)
- [S46]: https://github.com/Aider-AI/aider/issues/2525, https://github.com/Aider-AI/aider/issues/3314
- [S47]: https://github.com/continuedev/continue/blob/main/docs/customize/deep-dives/mcp.mdx
- [S48]: https://github.com/aws/amazon-q-developer-cli (crates/agent/src/agent/mcp/service.rs, crates/chat-cli/src/cli/agent/mcp_config.rs, crates/chat-cli/src/cli/agent/mod.rs)

### Zed (2026-09-21, source anchored at main@85b8198e)

- [Z1]: https://zed.dev/docs/ai/mcp (MCP docs page; in-repo source docs/src/ai/mcp.md)
- [Z2]: https://github.com/zed-industries/zed/blob/main/crates/settings_content/src/project.rs (context_servers / context_server_timeout schema)
- [Z3]: https://github.com/zed-industries/zed/blob/main/crates/project/src/context_server_store.rs (store, state machine, lifecycle, OAuth state transitions)
- [Z4]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/types.rs (hand-written MCP types)
- [Z5]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/transport.rs plus transport/stdio_transport.rs, transport/http.rs
- [Z6]: https://github.com/zed-industries/zed/blob/main/crates/context_server/Cargo.toml (no MCP SDK dependency)
- [Z7]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/client.rs (JSON-RPC client, default 60s timeout)
- [Z8]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/protocol.rs (protocol version negotiation)
- [Z9]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/oauth.rs (PKCE/DCR/CIMD/keychain)
- [Z10]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/tools/context_server_registry.rs (tool registration, mcp_tool_id, list_changed)
- [Z11]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/thread.rs (tool-name sanitization/collision prefix/64 cap/turn snapshot)
- [Z12]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/thread.rs (authorize_third_party_tool approval)
- [Z13]: https://github.com/zed-industries/zed/blob/main/crates/settings_content/src/agent.rs (tool_permissions schema; MCP gets no pattern support)
- [Z14]: https://zed.dev/docs/ai/tool-permissions
- [Z15]: https://github.com/zed-industries/zed/blob/main/assets/settings/default.json (the default profile's context-server switches)
- [Z16]: https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs (newSession forwards MCP servers)
- [Z17]: https://zed.dev/docs/ai/external-agents (native MCP config belongs to the external agent)
- [Z18]: https://zed.dev/docs/extensions/mcp-extensions (extension deprecation plan, issue #59351)
- [Z19]: https://github.com/zed-industries/zed/blob/main/crates/settings_ui/src/pages/mcp_servers_page.rs (settings-page UI)
- [Z20]: https://github.com/zed-industries/zed/blob/main/crates/migrator/src/migrations/m_2026_02_04/settings.rs (always_allow_tool_actions → tool_permissions.default)
- [Z21]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/native_agent_server.rs (Zed Agent as an ACP agent server)
- [Z22]: https://zed.dev/docs/extensions/agent-servers (ACP extension deprecation)

### Config interop (2026-09-21)

- [I1]: https://code.claude.com/docs/en/mcp (the Claude Code MCP reference in full: scopes/fields/variable expansion/import capabilities)
- [I2]: https://code.claude.com/docs/en/settings (verified that settings.json has no mcpServers key)
- [I3]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (the current full RawMcpServerConfig set)
- [I4]: https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs (global keys)
- [I5]: https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/rmcp_client.rs (DEFAULT_STARTUP_TIMEOUT=30s / DEFAULT_TOOL_TIMEOUT=300s)
- [I6]: https://zcode.z.ai/en/docs/mcp-services (Import From An External Agent / precedence rules)
- [I7]: local first-hand guides such as `~/.zcode/cli/plugins/cache/zcode-plugins-official/zcode-guide/0.2.0/skills/diagnosing-mcp/SKILL.md` (2026-09-21)
- [I8]: https://github.com/continuedev/continue/blob/main/docs/customize/deep-dives/mcp.mdx (directory-level direct read of Claude-family JSON)
- [I9]: https://opencode.ai/docs/mcp-servers/ (v1 docs, no import; `.well-known/opencode` organization distribution)
- [I10]: https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/mcp.md (--mcp-config-file; url means http)
- [I11]: https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md (url/httpUrl discrimination, credential scrubbing)
- [I12]: https://github.com/charmbracelet/crush/blob/main/docs/config/README.md (Bash crushrc / Legacy JSON)
- [I13]: https://agent-plugins.org/plugin-authors/mcp-servers and the spec repo https://github.com/agentplugins/agent-plugins-spec (Agent Plugins 1.0.0 mcp.json closed schema)
- [I14]: https://github.com/nicobailon/pi-mcp-adapter (package page https://pi.dev/packages/pi-mcp-adapter; /mcp setup, host discovery off by default)
- [I15]: local cargo checkout `~/.cargo/git/checkouts/pi-core-rs-16822f4cb4a15bb1/` (c6ac601 / e4150c0) has no MCP module tree-wide; the git dependency in Holt's root Cargo.toml

### Star counts and repo status notes

Star counts for each repo (codex 122,794; pi 103,437; opencode 206,117; gemini-cli 106,878; cline 67,728; OpenHands 87,055; goose 54,060; aider 48,853; continue 35,844; crush 27,972; qwen-code 27,723; Roo-Code 24,305; kimi-cli 11,335; amazon-q-developer-cli 1,983) were queried via the GitHub API on 2026-09-10; opencode (sst→anomalyco), goose (block→aaif-goose), and pi (badlogic/pi-mono→earendil-works/pi) are all the current locations after API redirects.
