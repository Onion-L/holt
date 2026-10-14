# Survey of Web Fetch / Web Search Tool Implementations in Coding Agents

> Survey date: 2026-09-10. Written for engineers, this document maps how mainstream coding agents implement the two model tools web fetch (retrieving a URL) and web search (internet search). Background: Holt plans to add these two tools to its engine; Holt's agent loop comes from pi-core-rs, and the pi ecosystem deliberately ships no web tools, so we need to review industry practice before settling on a design. Five samples: Claude Code (closed-source but thoroughly documented), the source-inspectable Codex / opencode / Kimi Code CLI, and upstream pi. All conclusions come with source-level evidence wherever possible; volatile details (version numbers, defaults, behavior switches) are as of the survey date.

## 1. Conclusions up front

- **Two ends of the spectrum**: pi deliberately ships no web tools ("no web search/fetch tool by default; the model can use curl"), pushing web capability out to skills / extensions; Codex sits at the other extreme — zero client-side HTTP fetching, with search and page opening done entirely server-side. The middle mainstream (Claude Code, opencode, Kimi) all do **client-side local fetching + HTML→text/markdown + output truncation**.
- **None of the search backends builds its own crawler index**: Claude Code is tied to the Anthropic API's server-side web_search (backend not swappable), Codex is tied to the OpenAI backend (`alpha/search` endpoint), opencode outsources to Exa / Parallel hosted MCPs (a hand-written thin JSON-RPC client), and Kimi uses its own platform's private `/search` endpoint. The tools themselves are all thin; the heavy assets live in the backend — the lesson for Holt is that the search tool should be a **pluggable backend + absent when unconfigured**.
- **Two parameter philosophies for fetch**: `url` only, returning the full text (opencode, Kimi; the model reads it itself) vs `url + prompt`, where a small model distills an answer (Claude Code only, "lossy by design" — saves context but loses fidelity). Codex has no fetch tool; opening/paging are commands of the search backend.
- **HTML→text conversion is a per-project choice**: opencode uses turndown (JS) plus a hand-written htmlparser2 traversal, Kimi uses trafilatura (Python; the only one approximating readability-style main-content extraction), Codex's Go-era predecessor used goquery + html-to-markdown; the pi ecosystem (including pi-core-rs) has no ready-made implementation at all, so the Rust side must pick its own library.
- **Limits are a shared convention**: download cap (opencode's 5MB, double-checked), 50KB output cap (opencode's generic truncation, overflow spilled to disk; the same 50KB cap as Holt's `grep`), 50k characters (Kimi), timeouts of 30s default / 120s max (opencode), 180s (Kimi). Writing the limits into the tool description is consistent practice everywhere (especially in the pi family).
- **Permission defaults fall into two camps**: Claude Code asks on every call by default, but has fine-grained `WebFetch(domain:...)` rules and preapproved documentation domains; opencode (`"*": "allow"` baseline), Kimi (outside the approval system), and Codex ("no per-call approval") let calls through silently by default. Fetch is read-only but sends outbound network traffic; the default tier is a product decision.
- **Safety measures are uneven and generally weak**: only Claude Code does an outbound preflight (sending the hostname to api.anthropic.com to check a safety blocklist before fetching); opencode / Kimi neither read robots.txt nor keep domain allow/deny lists nor have SSRF/intranet protection; UA strategy splits three ways — identify itself (Claude-User), spoof Chrome first and identify itself once Cloudflare catches it (opencode), spoof Chrome (Kimi).
- **Graceful degradation has proven patterns**: Kimi's `SkipThisTool` (service unconfigured → the tool simply disappears from the manifest) and opencode's system-prompt fallback (when there is no search tool, teach the model to fetch a search-engine URL with webfetch) are both worth copying; Claude's "backend not configurable, add an MCP server to switch" is a counterexample (for multi-provider Holt).

## 2. Product-by-product teardown

### 2.1 Claude Code (closed-source; primary sources = official docs + changelog)

**WebFetch** — a client-side local tool, parameters `url` + `prompt` [S1]:

- Fetches the page, converts HTML→Markdown (internal implementation, not configurable), then "runs the prompt over the content with a small, fast model" — Claude receives the distilled answer rather than the original text; the docs say outright that "This makes WebFetch lossy by design", and raw content requires Bash curl [S1].
- HTTP auto-upgrades to HTTPS; large pages are truncated to a fixed character cap before processing; cross-host redirects are not followed automatically — the redirect information is returned so Claude can issue a second call; 529/rate-limit responses retry with backoff [S1].
- **15-minute cache**; from v2.1.233+ adjustable via `CLAUDE_CODE_WEBFETCH_CACHE_TTL_MS` (default 900000ms, read once at startup) [S1][S3].
- The UA starts with `Claude-User`, and the Accept header negotiates content preferring Markdown; the v2.1.83 changelog's stated rationale is "identifying itself lets site owners recognize/allow it via robots.txt" — **the tool itself neither reads nor honors robots.txt** [S1][S6].
- **Outbound safety preflight**: before fetching, the hostname (not the full URL) is sent to `api.anthropic.com` to check against a safety blocklist; hostnames that pass are cached for 5 minutes. It is independent of the model provider, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` cannot turn it off, and the only switch is the settings key `skipWebFetchPreflight: true` [S4].
- Permissions: ask on every call by default (Manual / acceptEdits modes), except for domains already covered by rules and a built-in set of preapproved documentation domains; "Yes, and don't ask again" writes a `WebFetch(domain:...)` allow rule into the repo's `.claude/settings.local.json`. Rule syntax: `WebFetch(domain:example.com)`, `WebFetch(domain:*.example.com)` (matches subdomains, not the bare domain), `WebFetch(domain:*)`, bare `WebFetch`; denying the bare name = removes the tool outright, deny `domain:*` = keeps the tool but rejects every call; `domain:` rules also sync into the sandbox network allow/deny lists; explicit rules take precedence over the preapproved set [S2].

**WebSearch** — the backend is the Anthropic API's server-side `web_search`:

- "Executes queries against Anthropic's web search backend and returns result titles and URLs, **without fetching the result pages**" (reading a page requires a follow-up WebFetch call); "The search backend is not configurable. To search with a different provider, add an MCP server" [S1].
- API-level parameters: `max_uses`, `allowed_domains`/`blocked_domains` (mutually exclusive), `user_location`; billed at $10 per 1000 calls plus tokens [S5]. On the Claude Code side, a single call issues at most 8 backend searches; **per-session cap of 200 (main conversation + subagents combined)** (v2.1.212+; `CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION` is adjustable but cannot be turned off, `/clear` resets it; at the limit it returns a "continue with the information you already have" prompt instead of erroring) [S1][S3].
- Permissions likewise default to ask, but the rule has a single form, bare `WebSearch` (no domain granularity); Amazon Bedrock does not expose this server-side tool; the current docs no longer carry the early "US-only" wording [S1][S2].
- v2.1.248's `--restricted` mode removes WebFetch (unless named via `--tools`) [S6].

### 2.2 OpenAI Codex (Rust; commit `b348fc266`, 2026-09-10)

Two web paths, **no standalone client-side fetch tool**; both execute server-side [S8][S10][S11]:

1. **Hosted `web_search`** (a Responses API hosted tool): defined in `tools/src/tool_spec.rs:39-53`, carrying `external_web_access`, `indexed_web_access`, `filters` (allowed_domains), `user_location`, `search_context_size`, `search_content_types`, serialized directly into the request's `tools` array. The model issues no function call; the API server executes and streams back `WebSearchCall { id, status, action }` (`protocol/src/models.rs:1178-1192`; action splits into search/open_page/find_in_page), and the client only maps it into events at `core/src/event_mapping.rs:229-240`, with the TUI rendering "Searching/Searched the web" [S8][S9].
2. **Standalone `web.run` extension tool** (new crate `ext/web-search`): exposed as an ordinary function tool, but its execution body does no local fetching — it packs recent conversation snippets + commands and POSTs them to the OpenAI backend `alpha/search` (`codex-api/src/endpoint/search.rs:31-33`; `ext/web-search/src/tool.rs:90-110`); searching, opening pages, paging, and screenshots all happen server-side, and the `ref_id` (of the turn0search0 form) is server-side session state. The parameter schema is generated from `SearchCommands` via schemars: `search_query: [{q, recency?, domains?}]` (at most 4 per call), `open: [{ref_id|URL, lineno?}]`, `click`, `find`, etc.; the description text lives in `web_run_description.md` (pulled in via include_str!) and spells out the decision boundary for "when browsing is required", the citation format, and the 25-word verbatim-quotation cap [S10][S11].

The two are mutually exclusive (when standalone is available, the hosted tool is not sent, `core/src/tools/spec_plan.rs:609-616`); standalone also requires a provider capability declaration + a feature flag that is off by default (`spec_plan.rs:1038-1047`) [S12].

**Configuration and permissions** [S13][S14][S15]:

- Top-level config.toml `web_search = "disabled"|"cached"|"indexed"|"live"` (default **Cached**: the tool is injected but `external_web_access=false`, reading only from cache without network); `tools.web_search = { context_size, allowed_domains, location }`; the `--search` flag is exactly `web_search="live"`, and its help text says "no per-call approval" outright. The old name `web_search_request` is deprecated.
- No per-call approval; there are only two hard gates: the provider capability declaration, and the review subagent forcing `WebSearchMode::Disabled` (`core/src/tasks/review.rs:107-114`).

### 2.3 opencode (TS; commit `859106eb`, dev branch)

**webfetch** (`packages/opencode/src/tool/webfetch.ts`) — a pure HTTP client with **no LLM distillation step whatsoever** (the "Results may be summarized" in the tool description is leftover copy; no historical tag ever had summarize behavior either) [S16][S20]:

- Parameters: `url`, `format` (`"text"|"markdown"|"html"`, default markdown), `timeout` (default 30s, max 120s); there is no prompt parameter — the full text is returned directly. The description claims HTTP auto-upgrades to HTTPS, but the code only validates the prefix and contains no upgrade logic (a docs-implementation gap) [S16].
- Native fetch (Effect `FetchHttpClient`); Content-Length and the actual body are both checked, and **anything over 5MB throws immediately**; by default it spoofs a Chrome 143 UA, and on a 403 with `cf-mitigated: challenge` (a Cloudflare bot challenge) it retries once with the honest UA `"opencode"` [S16].
- HTML→markdown uses **turndown** (atx headings, fenced code, strips script/style/meta/link); HTML→text uses a hand-written htmlparser2 traversal that skips script/style/noscript/iframe and the like; no readability-style main-content extraction; non-HTML content is returned as-is; images (non-svg) are converted to base64 data-URLs and returned as attachments; no authorization headers are sent [S16].

**websearch** (`websearch.ts` + `mcp-websearch.ts`) — a built-in tool, but fully outsourced [S17][S18]:

- Sends hand-written JSON-RPC `tools/call` to two hosted MCPs, Exa (`https://mcp.exa.ai/mcp`) and Parallel (`https://search.parallel.ai/mcp`) (25s timeout, SSE parsing); parameters: `query` (required) + `numResults` (default 8), `livecrawl`, `type`, `contextMaxCharacters` (the latter four apply to Exa only).
- Provider selection: forceable via env/flags; **with neither set, traffic is split 50/50 between the two by sessionID checksum modulo**.
- Exposure condition (registry `registry.ts:58-65`): only when the provider is opencode's own gateway, or when `OPENCODE_ENABLE_EXA` / `OPENCODE_ENABLE_PARALLEL` is set; other providers do not get it, and the system-prompt fallback teaches the model to "use webfetch to fetch `https://www.google.com/search?q=…`" (`src/session/prompt/beast.txt:68`) [S18][S19].

**Permissions and truncation** [S19]:

- Both tools go through `ctx.ask` before execution: permissions are `webfetch` (by URL) / `websearch` (by query) respectively, and the default agents baseline `"*": "allow"` lets them through silently; users can allow/ask/deny by wildcard, and deny `*` removes the tool from the request outright; an "always" answer to an ask writes `["*"]`, letting everything through for the rest of the session; the `explore` subagent explicitly allowlists these two tools and denies everything else.
- Uniform output truncation: past 2000 lines or 50KB it keeps a head preview, spills the full text to `<data>/tool-output/`, and points the model at Grep/Read (`tool.ts:131-145`, `truncate.ts:14-15`).

Historical comparison: the Go predecessor (v0.0.1, `internal/llm/tools/fetch.go`) used goquery + JohannesKaufmann/html-to-markdown, structurally identical to today's — **webfetch has always been a pure fetcher; fetch+summarize never existed** [S21].

### 2.4 Kimi Code CLI (Python; commit `86f1364`, v1.50.0)

Only two tools: `SearchWeb` (`src/kimi_cli/tools/web/search.py`) and `FetchURL` (`fetch.py`), carried by default by the main agent and all subagents [S22][S23]:

- **SearchWeb**: the backend is Moonshot's in-house search service — POST `https://api.kimi.com/coding/v1/search` (overridable via `KIMI_CODE_BASE_URL`), body `{text_query, limit, enable_page_crawling, timeout_seconds: 30}`; credentials reuse the login state (API key or OAuth token), client timeout total=180s. `include_content=True` maps to the server-side `enable_page_crawling` (the server crawls back the full page text). The `search_results[]` response is rendered as Title/Date/URL/Summary text blocks. **When the service is unconfigured, the constructor throws `SkipThisTool` and the tool silently disappears from the model manifest** (`soul/toolset.py:694-699` catches it and continues) [S23][S24].
- **FetchURL** is dual-path: it prefers the Moonshot Fetch service (POST `/coding/v1/fetch`, `Accept: text/markdown`), and falls back to local aiohttp + **trafilatura** main-content extraction on failure (`include_comments/tables=True`; output is plain text, not markdown); the UA spoofs Chrome 91, total timeout 180s, `text/plain|markdown` passes through as-is, and a failed extraction returns "the page may require JavaScript rendering". Results are hard-truncated at **50,000 characters** with an `[...truncated]` marker [S23].
- Parameters: `SearchWeb(query, limit 1–20 default 5, include_content default False)` (the description nudges the model to rewrite the query rather than raise the limit); `FetchURL(url)` — that single parameter only [S23].
- **Zero protection**: no robots.txt, no domain allow/deny lists, no SSRF protection, no redirect limits — the HTTP status code is the only gate; **neither tool is in the approval system**, and both execute without confirmation by default (contrast Shell/file writes, which require approval) [S23].
- Configuration: `[services.moonshot_search]` / `[services.moonshot_fetch]` (base_url, api_key, oauth), written automatically by `/login` (OAuth) or `/setup` (API key); the official docs say outright that these two tools are currently provided only by the Kimi Code platform — under other providers SearchWeb is unavailable and FetchURL falls back to local fetching [S24][S25].
- A decoupling fact: the Kimi open platform's K2 series has a `$web_search` builtin_function at the API layer (server-side execution, billed per call), but kimi-cli deliberately does not use it, going through the platform sidecar endpoint instead — agent logic stays decoupled from the model vendor [S26].

### 2.5 pi / pi-core-rs (deliberately not built)

- Upstream TS version (earendil-works/pi, formerly badlogic/pi-mono; commit `bbb61e34`, 2026-09-10): the built-in tools are only bash/edit/find/grep/ls/powershell/read/write — **there are no web fetch/search/browser tools at all, nor an official extension example**. The README names the default four read/write/edit/bash; the blog's exact words: "By default, pi has no web search or fetch tool. However, it can use curl or read files from disk." The philosophy is "minimal core, build the rest with extensions/skills/third-party packages", with a firm refusal to do MCP [S27][S28].
- The official outlet for web capability is **skills**: `docs/skills.md:188-231` gives a complete `brave-search` skill example (bash script + Brave Search API) and points to the official skill repo pi-skills ("Web search, browser automation, …"). The extension point for custom tools is `pi.registerTool({ name, label, description, parameters, execute })` [S29][S30].
- The Rust version pi-core-rs (commit `c6ac601`, v0.84.4): only read/write/edit/bash were ported (including image handling); even grep/find/ls are absent; `src/agent/search/` is conversation-history search, unrelated to the web; the reqwest dependency serves only the provider transport layer [S31].
- **Tool extension interface** (the part that matters most to an integrator): no trait — it is data + closures; the low-level `AgentTool { name, label, description, parameters: JSON schema, execute: Arc<dyn Fn(…)->BoxFuture<…>> }` (`src/agent/types.rs:300-317`); the high-level `AgentHarnessTool` adds an opaque context (built-in tools downcast it to `ExecutionToolContext { env }`). **Adding a new web tool does not require ExecutionEnv** — just construct an `AgentTool` directly; pi-core-rs ships a reusable HTTP foundation (`src/ai/utils/http.rs`'s `HttpFetch` trait, cancellation-aware `HttpRequest`, proxy-aware `ReqwestFetch`), but it is used only by the provider layer [S31][S32].
- Stating truncation/size limits in tool descriptions is consistent pi tool-design practice (the read tool and the official rg example both do it) [S31].

## 3. Side-by-side comparison

| Dimension | Claude Code | Codex | opencode | Kimi CLI | pi / pi-core-rs |
| --- | --- | --- | --- | --- | --- |
| Fetch tool | WebFetch (local fetch) | None (open command is server-side; curl as fallback) | webfetch (local fetch) | FetchURL (server-side first + local fallback) | None (bash+curl) |
| Fetch distillation | Small model distills per prompt, lossy | — | None, full text | None, full text | — |
| HTML processing | Internal HTML→MD | — | turndown / htmlparser2 | trafilatura→plain text | — |
| Search tool | WebSearch | Hosted `web_search` + `web.run` | websearch (conditionally exposed) | SearchWeb (disappears when unconfigured) | None |
| Search backend | Anthropic API server-side (not swappable) | OpenAI server-side | Exa/Parallel hosted MCP | Kimi Code platform private endpoint | — |
| Search reads pages | No (paired with WebFetch) | Yes (open/click) | No (paired with webfetch) | Optional (include_content) | — |
| Download/output limits | Large pages truncated before processing | Server-side output budget | 5MB hard cap; output 2000 lines/50KB spilled to disk | Output truncated at 50k characters | — |
| Caching | 15 minutes (adjustable TTL) | — | None | Server-side | — |
| robots.txt | Not honored (UA identifies itself) | — | No | No | — |
| SSRF/domain protection | Outbound blocklist preflight + domain rules | Server-side responsibility | None | None | — |
| UA strategy | `Claude-User` | — | Chrome 143→identifies itself on 403 | Chrome 91 | — |
| Permission default | Ask (domain rules/preapproved can allow through) | No per-call approval | Allow (narrowable via URL/query rules) | No approval | — |
| When unconfigured | Tool still present | Cached mode present by default | Conditional registration | Disappears via SkipThisTool | — |

## 4. Implementation recommendations for Holt

Drawing on the five implementations and Holt's current state (agent loop from pi-core-rs; tool assembly in `execution_tools_for_model` at `crates/engine/src/tools.rs:615`; the in-house `grep` tool is a ready-made holt-native `AgentTool` template; the ADR-permission-modes-gate-mutations-no-os-sandbox permission gate already covers write/edit/bash; the workspace already has reqwest 0.12):

1. **Implementation layer: holt-native `AgentTool`, zero changes to pi-core-rs.** Model it on `tools/grep.rs` (limits stated in the DESCRIPTION, `Deserialize` input struct, 50KB output cap, cancellation token threaded throughout): add `tools/web_fetch.rs` and `tools/web_search.rs` and append them in `execution_tools_for_model`. pi-core-rs's `AgentTool` is just data + closures and needs no ExecutionEnv.
2. **Fetch takes the "return the full text" route (opencode/Kimi); no small-model distillation in v1.** Parameter `url` (+ optional `format: text|markdown`, default markdown); rationale: distillation costs an extra provider round trip and is lossy, Holt's grep/read style has always been "state the limits clearly and let the model read", and full-text truncation is more predictable when the UI renders `MessagePart`s. HTML→markdown needs a new Rust dependency (htmd, readability-class candidates, **unverified — needs a PoC**); the plain-text fallback can borrow opencode's hand-written html5ever traversal approach.
3. **Fetch hard limits aligned with existing convention**: 50KB output cap (matching `grep.rs`'s `OUTPUT_BYTE_CAP`), download cap around 5MB (opencode), 30s default timeout with an adjustable ceiling, cancellation token from the run carried throughout (grep already demonstrates the spawn_blocking + select pattern; fetch is pure async and simpler). Self-identifying UA (`holt/<version>`), http(s) only; **add the SSRF protection the industry broadly lacks** (reject loopback/intranet/link-local addresses) — cheap differentiation. None of the five honor robots.txt; not recommended for v1.
4. **Search as a pluggable backend; the tool does not appear when unconfigured.** Define an internal search-backend trait (REST APIs like Brave/Exa/Tavily, or follow opencode straight to a hosted MCP — a thin client suffices); mirror Kimi's `SkipThisTool` semantics: no backend configured → do not register the tool, rather than registering and then erroring. **Do not** hard-bind to any one model provider's server-side search (holt is multi-provider; Claude Code's "backend not configurable" is a proven anti-pattern). Output a title/url/snippet list and leave page reading to fetch — the search/fetch division of responsibility is the shared split in Claude Code/opencode.
5. **Permissions behind the ADR-permission-modes-gate-mutations-no-os-sandbox gate.** Fetch is read-only but sends outbound network traffic, and so is search; recommend not gating by default (same tier as grep), with a settings-level interface for domain deny rules (borrowing Claude's `domain:` syntax and opencode's wildcard matching); converging the behavior through PermissionMode can be a later iteration. **Settle the default tier before shipping**: three of the four let calls through by default; only Claude Code asks on every call.
6. **Optional fallback when no search backend is configured**: follow opencode and teach the model in the system prompt to "use webfetch to fetch a search-engine URL"; also, pi's reminder still holds — bash+curl has always been the universal fallback, and a dedicated tool's incremental value is structured output, controllable permissions, and not depending on the user's machine having curl — the tool description should spell out those three points of value.

## 5. Sources

### Claude Code

- [S1]: https://code.claude.com/docs/en/tools-reference (WebFetch/WebSearch behavior, cache, limits, redirects)
- [S2]: https://code.claude.com/docs/en/permissions (`domain:` rule syntax, default approval, preapproved)
- [S3]: https://code.claude.com/docs/en/env-vars (CLAUDE_CODE_WEBFETCH_CACHE_TTL_MS, CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION)
- [S4]: https://code.claude.com/docs/en/data-usage#webfetch-domain-safety-check (hostname blocklist preflight, skipWebFetchPreflight)
- [S5]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool (API server-side parameters and billing)
- [S6]: https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md (v2.1.83 Claude-User UA, v2.1.248 --restricted, v2.1.212 limits)

### Codex (commit `b348fc26674189f758d5941cdab3f78f258b2aa7`, paths relative to codex-rs/)

- [S7]: https://github.com/openai/codex
- [S8]: tools/src/tool_spec.rs:39-53 (ToolSpec::WebSearch fields)
- [S9]: protocol/src/models.rs:1178-1192 (WebSearchCall); core/src/event_mapping.rs:229-240; tui/src/history_cell/search.rs:8-12
- [S10]: ext/web-search/src/tool.rs:40-110; ext/web-search/src/web_run_description.md; ext/web-search/src/schema.rs:6-35
- [S11]: codex-api/src/endpoint/search.rs:31-33 (alpha/search); codex-api/src/search.rs:32-78, 298-305
- [S12]: core/src/tools/spec_plan.rs:596-633, 1038-1047, 609-616; core/src/tools/hosted_spec.rs:14-46; features/src/lib.rs:1074-1078
- [S13]: protocol/src/config_types.rs:375-382 (default Cached); core/src/config/mod.rs:3699-3700; config/src/config_toml.rs:445-446
- [S14]: tui/src/cli.rs:68-70 (--search and "no per-call approval"); cli/src/main.rs:2309-2314
- [S15]: core/src/tasks/review.rs:107-114 (review forces Disabled)

### opencode (commit `859106eb17d5b840475f5e4b78e64c9622f8750e`, now at anomalyco/opencode)

- [S16]: packages/opencode/src/tool/webfetch.ts (full file; parameters/UA/5MB/turndown/htmlparser2)
- [S17]: packages/opencode/src/tool/websearch.ts; packages/opencode/src/tool/mcp-websearch.ts
- [S18]: packages/opencode/src/tool/registry.ts:58-65, 218-249, 292-295; src/config/runtime-flags.ts:31-40
- [S19]: packages/opencode/src/agent/agent.ts:119-134, 199-212; src/permission/index.ts:28-38, 143-151, 204-214; src/session/tools.ts:81-88; src/session/llm/request.ts:207-213; packages/opencode/src/tool/truncate.ts:14-15, 127-137; src/session/prompt/beast.txt:68
- [S20]: packages/web/src/content/docs/tools.mdx:254-286; docs/permissions.mdx:161-162
- [S21]: tag v0.0.1 internal/llm/tools/fetch.go; tag v0.1.0 packages/opencode/src/tool/webfetch.ts (historical comparison)

### Kimi Code CLI (commit `86f136422a0aae6b217ea49e7ea1d2e8a1defcd2`, v1.50.0)

- [S22]: https://github.com/MoonshotAI/kimi-cli; src/kimi_cli/agents/default/agent.yaml (allowed_tools)
- [S23]: src/kimi_cli/tools/web/search.py; src/kimi_cli/tools/web/fetch.py; src/kimi_cli/tools/utils.py:50 (50k truncation); src/kimi_cli/soul/toolset.py:694-699 (SkipThisTool)
- [S24]: src/kimi_cli/auth/platforms.py:64 (api.kimi.com endpoint); src/kimi_cli/config.py:128-168; src/kimi_cli/auth/oauth.py:595-608; docs/en/configuration/providers.md:142-153
- [S25]: pyproject.toml (trafilatura==2.0.0, aiohttp==3.13.3)
- [S26]: https://platform.kimi.ai/docs/guide/use-web-search (K2 `$web_search` builtin_function)

### pi / pi-core-rs

- [S27]: https://github.com/earendil-works/pi (formerly badlogic/pi-mono; commit `bbb61e34`; packages/coding-agent/src/core/tools/, README.md:91, 495-499)
- [S28]: https://mariozechner.at/posts/2025-11-30-pi-coding-agent/ ("By default, pi has no web search or fetch tool")
- [S29]: pi repo docs/skills.md:188-232 (brave-search skill example)
- [S30]: https://github.com/badlogic/pi-skills (official skill repo for web capabilities); packages/coding-agent/examples/extensions/truncated-tool.ts:49 (registerTool example)
- [S31]: https://github.com/Onion-L/pi-core-rs (commit `c6ac601`, v0.84.4; local checkout `~/.cargo/git/checkouts/pi-core-rs-16822f4cb4a15bb1/`; src/agent/types.rs:300-317, src/agent/harness/tools/read.rs:38-67, src/ai/utils/http.rs:114-119, src/ai/utils/reqwest_fetch.rs:16)
- [S32]: pi-core-rs src/agent/harness/types.rs:79-92; src/agent/harness/tools/tool_context.rs:14-17

### Holt's own repo (implementation anchors)

- crates/engine/src/tools.rs:615 (`execution_tools_for_model` tool assembly), :590-605 (`with_execution_context`)
- crates/engine/src/tools/grep.rs (holt-native AgentTool template: DESCRIPTION/50KB output/cancellation)
- crates/engine/src/gate.rs (ADR-permission-modes-gate-mutations-no-os-sandbox permission gate: pending Approval, PermissionMode)
- Root Cargo.toml:84 (reqwest 0.12, rustls)
