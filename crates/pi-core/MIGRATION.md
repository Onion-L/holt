# Migration checklist

Tracking for the TypeScript → Rust port of `@earendil-works/pi-telemetry`,
`@earendil-works/pi-ai`, and `@earendil-works/pi-agent-core` v0.84.4.

Status legend:

- **pending** — not yet ported.
- **done** — ported with its applicable tests passing.
- **partial** — the ported surface passes its tests, but a named capability
  gap remains (the row says exactly what is missing). A partial row must
  close — by porting the gap or reclassifying it as a documented
  language/runtime deviation — before the migration is complete.
- **deferred** — deliberately postponed with the reason recorded in the row;
  same closure requirement as partial.
- **live** — the TypeScript suite is credential-gated (env API keys or OAuth
  tokens, same `skipIf` conditions listed in the row). The mapped Rust entry
  uses the same gate, returns successfully without credentials, and performs
  the real provider request when credentials are available.
- **exception** — ported with a documented, minimal deviation (the note says
  where the deviation is documented), or deliberately out of scope.

Out of scope per `AGENTS.md`: the `node:sqlite` session backend (a separate
upstream package).

## Compatibility baseline

The statuses below describe v0.84.4, not v0.85.0. The local AI oracle was
observed at v0.85.0 during the 2026-09-06 audit, while the committed oracle
manifest and the agent/telemetry sources still identify v0.84.4. Consequently,
oracle integrity and TypeScript export checks currently fail. Do not regenerate
the manifest merely to clear those failures or claim v0.85.0 compatibility from
the Rust test results.

A v0.85.0 AI migration still needs an explicit target change and parity work
for the replacement Cloudflare AI binding API, assistant-message frames,
timestamped UUIDs, per-turn Anthropic effort/thinking recovery, new OpenAI
compatibility options, and the remaining upstream behavior/test changes. The
newer model catalog alone does not complete that migration.

Offline maintenance verification uses `PI_TEST_OFFLINE=1 cargo test --all-targets`.
This explicitly disables live credentials (including local OAuth files) and
local model services in test helpers; live scenarios remain present and report
the opt-out reason. Provider lifecycle regressions in `tests/ai_stream_lifecycle.rs`
also verify delivery before EOF and cancellation while the response body is
pending. These maintenance fixes do not change the compatibility target.

## pi-telemetry

### Source modules

| TypeScript source | Rust module | Status |
|---|---|---|
| `src/index.ts` | `src/telemetry/mod.rs` | done |
| `src/noop.ts` | `src/telemetry/noop.rs` | done |
| `src/memory.ts` | `src/telemetry/memory.rs` | done |
| `src/testing/index.ts` | `src/telemetry/testing/mod.rs` | done |
| `src/testing/types.ts` | `src/telemetry/testing/types.rs` | done |
| `src/testing/conformance.ts` | `src/telemetry/testing/conformance.rs` | done |
### Tests

| TypeScript test | Rust test | Status |
|---|---|---|
| `test/telemetry.test.ts` | `tests/telemetry.rs` | done |
| `test/conformance.test.ts` | `tests/telemetry_conformance.rs` | done |
### Documented deviations

- TypeScript callback rejections map to `Result` errors on the Rust `try_*`
  wrappers; automatic error-status names use the short Rust type name instead
  of `error.name` (documented in `src/telemetry/mod.rs`).
- JS `Proxy`-based passivity cases are adapted to plain-value equivalents
  (documented inline in `src/telemetry/testing/conformance.rs`).
- TypeScript conditional-type vocabulary checking (`SchemaTelemetrySpan`,
  `InferStartAttributes`, …) is compile-time only; the runtime behavior of
  `createTypedSpanStarter` is ported, the type-level checks have no Rust
  equivalent (documented in `src/telemetry/mod.rs`).

## pi-ai

### Source modules

| TypeScript source | Rust module | Status |
|---|---|---|
| `src/api/anthropic-messages.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/anthropic-messages.ts` | `src/ai/api/anthropic_messages.rs` | done |
| `src/api/azure-openai-responses.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/azure-openai-responses.ts` | `src/ai/api/azure_openai_responses.rs` | done |
| `src/api/bedrock-converse-stream.lazy.ts` | `src/ai/providers/apis.rs` | done (direct dispatch adapter; the Bun `setBedrockProviderModule` override has no Rust counterpart) |
| `src/api/bedrock-converse-stream.ts` | `src/ai/api/bedrock_converse_stream.rs` + `src/ai/utils/aws_credentials.rs` | done (SigV4 with the documented AWS vector, bearer/skip-auth/static/profile credential resolution, endpoint+region resolution, vnd.amazon.eventstream framing; the AWS SDK default-chain remote providers the TS adapter delegates to are ported in `aws_credentials.rs` — web identity (IRSA) via the STS `AssumeRoleWithWebIdentity` form call and ECS container credentials via the `169.254.170.2`/full-URI metadata endpoints, both cached until expiry) |
| `src/api/cloudflare-gateway-binding.ts` | `src/ai/api/cloudflare_gateway_binding.rs` | done (Request/init header merging and the binding run are ported; the request's abort signal forwards into the binding run options; the `signal: null`-clears case is unrepresentable — the Rust `HttpRequest` is the single final request form with no `Request`/`init` split) |
| `src/api/cloudflare.ts` | `src/ai/api/cloudflare.rs` | done |
| `src/api/constrained-sampling.ts` | `src/ai/api/constrained_sampling.rs` | done |
| `src/api/github-copilot-headers.ts` | `src/ai/api/github_copilot_headers.rs` | done |
| `src/api/google-generative-ai.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/google-generative-ai.ts` | `src/ai/api/google_generative_ai.rs` | done (stream/event mapping, headers, and retry ported; a provided custom `fetch` rejects with the TypeScript message — tests inject through `stream_with_transport`, the mocked-SDK seam; passing the ambient fetch explicitly has no Rust counterpart) |
| `src/api/google-shared.ts` | `src/ai/api/google_shared.rs` | done (`mapStopReason`/`retryGoogleRequest` map to the public `map_stop_reason_string`/`retry_provider_request` facades) |
| `src/api/google-vertex.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/google-vertex.ts` | `src/ai/api/google_vertex.rs` | done (stream/event mapping and auth-header resolution ported; a provided custom `fetch` rejects with the TypeScript message — tests inject through `stream_with_transport`, the mocked-SDK seam; passing the ambient fetch explicitly has no Rust counterpart) |
| `src/api/lazy.ts` | `src/ai/providers/apis.rs` | done (lazy loading collapses to direct dispatch; the Node module-registry probe in `test/lazy-module-load.test.ts` has no Rust equivalent — disposition recorded with `src/providers/all.ts`) |
| `src/api/mistral-conversations.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/mistral-conversations.ts` | `src/ai/api/mistral_conversations.rs` | done |
| `src/api/openai-codex-responses.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/openai-codex-responses.ts` | `src/ai/api/openai_codex_responses.rs` + `src/ai/api/openai_codex_websocket.rs` | done (SSE path with request shape, URL resolution, retry policy, Codex event mapping, error taxonomy, and zstd request-body compression; WebSocket transport over `responses_websockets=2026-02-06` with the account-scoped session cache, idle/age expiry, continuation deltas, debug stats, connect/idle timeouts, and SSE fallback; default connector is tokio-tungstenite, injectable for tests) |
| `src/api/openai-completions.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/openai-completions.ts` | `src/ai/api/openai_completions.rs` | done |
| `src/api/openai-prompt-cache.ts` | `src/ai/api/openai_completions.rs` | done |
| `src/api/openai-responses-shared.ts` | `src/ai/api/openai_responses_shared.rs` | done |
| `src/api/openai-responses.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/openai-responses.ts` | `src/ai/api/openai_responses.rs` | done |
| `src/api/openrouter-images.lazy.ts` | `src/ai/providers/builtin.rs` | done (direct dispatch adapter) |
| `src/api/openrouter-images.ts` | `src/ai/api/openrouter_images.rs` | done (request/response handling, retry, and usage parsing ported; the abort signal rides on the request like the OpenAI-SDK signal wiring, and the transport rejects an already-aborted signal) |
| `src/api/pi-messages.lazy.ts` | `src/ai/providers/apis.rs` | done |
| `src/api/pi-messages.ts` | `src/ai/api/pi_messages.rs` | done (`PiMessagesResponseError` shape is the public `StreamFailure`, threaded into the error event) |
| `src/api/simple-options.ts` | `src/ai/api/simple_options.rs` | done |
| `src/api/transform-messages.ts` | `src/ai/api/transform_messages.rs` | done |
| `src/auth/context.ts` | `src/ai/auth/context.rs` | done |
| `src/auth/credential-store.ts` | `src/ai/auth/credential_store.rs` | done |
| `src/auth/helpers.ts` | `src/ai/auth/helpers.rs` | done |
| `src/auth/oauth/anthropic.ts` | `src/ai/auth/oauth/anthropic.rs` | done (loopback callback over tokio TcpListener; client id inlined decoded) |
| `src/auth/oauth/device-code.ts` | `src/ai/auth/oauth/device_code.rs` | done (tokio time drives the fake-timer tests deterministically) |
| `src/auth/oauth/github-copilot.ts` | `src/ai/auth/oauth/github_copilot.rs` | done (client id inlined decoded; rate-limit budget uses injectable clock) |
| `src/auth/oauth/kimi-coding.ts` | `src/ai/auth/oauth/kimi_coding.rs` | done (injectable transport/clock/env; tokio timeout reproduces the 30s request signal) |
| `src/auth/oauth/load.ts` | `src/ai/auth/oauth/load.rs` | done (static linking replaces dynamic imports; bundled-loader registration is a bundler concern with no counterpart) |
| `src/auth/oauth/oauth-page.ts` | `src/ai/auth/oauth/oauth_page.rs` | done |
| `src/auth/oauth/openai-codex.ts` | `src/ai/auth/oauth/openai_codex.rs` | done (browser callback over tokio TcpListener; `accountId` rides the credential extension map) |
| `src/auth/oauth/openrouter.ts` | `src/ai/auth/oauth/openrouter.rs` | done (loopback callback over tokio TcpListener; callback path uses UUIDv7 in place of crypto.randomUUID) |
| `src/auth/oauth/pkce.ts` | `src/ai/auth/oauth/pkce.rs` | done |
| `src/auth/oauth/radius.ts` | `src/ai/auth/oauth/radius.rs` | done (loopback callback over tokio TcpListener; credential `scope` rides the extension map) |
| `src/auth/oauth/xai.ts` | `src/ai/auth/oauth/xai.rs` | done (injectable transport + clock replace global fetch and `vi.setSystemTime`) |
| `src/auth/resolve.ts` | `src/ai/auth/resolve.rs` | done |
| `src/auth/types.ts` | `src/ai/auth/types.rs` | done |
| `src/bedrock-provider.ts` | | exception (Bun static-embed module object; the Rust adapter is `src/ai/providers/apis.rs::bedrock_converse_stream_api`) |
| `src/bun-oauth.ts` | | exception (Bun binary loader registration; `registerBundledOAuthFlowLoaders` has no Rust counterpart — flows link statically, documented in `src/ai/auth/oauth/load.rs`) |
| `src/cli.ts` | `src/ai/cli.rs` + `src/bin/pi-ai.rs` | done (tests/ai_cli.rs; golden fixtures from scripts/oracle/generate-cli-goldens.mts; auth.json is written with the platform default mode like `writeFileSync` without a mode) |
| `src/compat.ts` | `src/ai/compat.rs` | done (global api-provider registry incl. the public `registerBuiltInApiProviders`, registerFauxProvider, env-key-injected global stream/complete, deprecated catalog reads) |
| `src/compat/extension-oauth-types.ts` | `src/ai/compat.rs` | done |
| `src/env-api-keys.ts` | `src/ai/env_api_keys.rs` | done |
| `src/image-models.generated.ts` | `src/ai/models_generated.rs (embedded with models catalog)` | done |
| `src/image-models.ts` | `src/ai/models_generated.rs` | done (`image_model`/`image_provider_ids`/`image_models_for_provider`) |
| `src/images-api-registry.ts` | `src/ai/images.rs` | done (public registry: register/get with `sourceId`, api-mismatch wrapper, override semantics; the built-in OpenRouter entry is the default) |
| `src/images-models.ts` | `src/ai/images_models.rs` | done |
| `src/images.ts` | `src/ai/images.rs` | done (`generateImages` dispatches through the registry with the TS error text) |
| `src/index.ts` | `src/ai/mod.rs` | done |
| `src/legacy-api-aliases.ts` | `src/ai/compat.rs` | done (deprecated per-api stream aliases) |
| `src/model-catalog.ts` | `src/ai/model_catalog.rs` | done (identity helper; TS generics are compile-time only) |
| `src/models-store.ts` | `src/ai/models_store.rs` | done (exercised through the models-runtime refresh tests) |
| `src/models.generated.ts` | `src/ai/models_generated.rs (+ src/ai/data/models.generated.json via scripts/oracle/export-model-catalog.mts)` | done |
| `src/models.ts` | `src/ai/models.rs` | done |
| `src/oauth.ts` | `src/ai/compat.rs (type re-exports)` | done |
| `src/providers/all.ts` | `src/ai/providers/builtin.rs` | done (every per-provider factory is public, incl. the `radiusProvider` re-export; the 39 `*_MODELS` constants are an exception — aggregate catalog) |
| `src/providers/amazon-bedrock.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/amazon-bedrock.ts` | `src/ai/providers/amazon_bedrock.rs` | done |
| `src/providers/ant-ling.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/ant-ling.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/anthropic.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/anthropic.ts` | `src/ai/providers/anthropic.rs` | done |
| `src/providers/azure-openai-responses.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/azure-openai-responses.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/baseten.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/baseten.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/cerebras.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/cerebras.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/cloudflare-ai-gateway.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/cloudflare-ai-gateway.ts` | `src/ai/providers/cloudflare_ai_gateway.rs` | done |
| `src/providers/cloudflare-auth.ts` | `src/ai/providers/cloudflare_auth.rs` | done |
| `src/providers/cloudflare-stream.ts` | `src/ai/providers/cloudflare_stream.rs` | done (`resolveCloudflareModel` is public as `resolve_cloudflare_model`) |
| `src/providers/cloudflare-workers-ai.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/cloudflare-workers-ai.ts` | `src/ai/providers/cloudflare_workers_ai.rs` | done |
| `src/providers/deepseek.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/deepseek.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/faux.ts` | `src/ai/providers/faux.rs` | done (`createFauxCore` maps to `faux_provider` + `FauxProviderHandle`; the registration-with-unregister envelope is `CompatFauxRegistration`) |
| `src/providers/fireworks.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/fireworks.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/github-copilot.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/github-copilot.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/google-vertex.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/google-vertex.ts` | `src/ai/providers/google_vertex.rs` | done |
| `src/providers/google.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/google.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/groq.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/groq.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/huggingface.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/huggingface.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/images/register-builtins.ts` | `src/ai/images.rs` | done (public `registerBuiltInImagesApiProviders`; the TS module-load side effect maps to first-access init) |
| `src/providers/kimi-coding.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/kimi-coding.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/minimax-cn.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/minimax-cn.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/minimax.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/minimax.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/mistral.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/mistral.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/moonshotai-cn.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/moonshotai-cn.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/moonshotai.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/moonshotai.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/nvidia.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/nvidia.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/openai-codex.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/openai-codex.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/openai.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/openai.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/opencode-go.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/opencode-go.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/opencode.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/opencode.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/openrouter-images.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/openrouter.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/openrouter.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/qwen-token-plan-cn.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/qwen-token-plan-cn.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/qwen-token-plan-individual.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/qwen-token-plan-individual.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/qwen-token-plan.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/qwen-token-plan.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/radius-config.ts` | `src/ai/providers/radius_config.rs` | done (incl. `loadRadiusGatewayConfig`) |
| `src/providers/radius.ts` | `src/ai/providers/radius.rs` | done |
| `src/providers/together.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/together.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/vercel-ai-gateway.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/vercel-ai-gateway.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/xai.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/xai.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/xiaomi-token-plan-ams.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/xiaomi-token-plan-ams.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/xiaomi-token-plan-cn.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/xiaomi-token-plan-cn.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/xiaomi-token-plan-sgp.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/xiaomi-token-plan-sgp.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/xiaomi.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/xiaomi.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/zai-coding-cn.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/zai-coding-cn.ts` | `src/ai/providers/builtin.rs` | done |
| `src/providers/zai.models.ts` | `src/ai/data/models.generated.json` | done (generated catalog; see `src/ai/models_generated.rs`) |
| `src/providers/zai.ts` | `src/ai/providers/builtin.rs` | done |
| `src/session-resources.ts` | `src/ai/session_resources.rs` | done |
| `src/types.ts` | `src/ai/types.rs` | done |
| `src/utils/abort-signals.ts` | `src/ai/utils/abort.rs` | done |
| `src/utils/abort.ts` | `src/ai/utils/abort.rs` | done |
| `src/utils/deferred-tools.ts` | `src/ai/utils/deferred_tools.rs` | done |
| `src/utils/diagnostics.ts` | `src/ai/utils/diagnostics.rs` | done |
| `src/utils/error-body.ts` | `src/ai/utils/error_body.rs` | done |
| `src/utils/estimate.ts` | `src/ai/utils/estimate.rs` | done |
| `src/utils/event-stream.ts` | `src/ai/utils/event_stream.rs` | done |
| `src/utils/hash.ts` | `src/ai/utils/text.rs (short_hash)` | done |
| `src/utils/headers.ts` | `src/ai/utils/headers.rs` | done |
| `src/utils/json-parse.ts` | `src/ai/utils/json_parse.rs` | done |
| `src/utils/node-http-proxy.ts` | `src/ai/utils/node_http_proxy.rs` | done (env precedence, NO_PROXY matching, and SOCKS/PAC rejection ported; the resolver returns the URL serialization with the trailing-slash normalization, matching the TypeScript `URL` object) |
| `src/utils/overflow.ts` | `src/ai/utils/overflow.rs` | done |
| `src/utils/pi-user-agent.ts` | `src/ai/session_resources.rs (get_pi_user_agent)` | done |
| `src/utils/provider-env.ts` | `src/ai/utils/provider_env.rs` | done |
| `src/utils/provider-retry.ts` | `src/ai/utils/provider_retry.rs` | done |
| `src/utils/retry.ts` | `src/ai/utils/retry.rs` | done |
| `src/utils/sanitize-unicode.ts` | `src/ai/utils/sanitize_unicode.rs` | done |
| `src/utils/sleep.ts` | `src/ai/utils/abort.rs (abortable_sleep)` | done |
| `src/utils/text.ts` | `src/ai/utils/text.rs` | done |
| `src/utils/typebox-helpers.ts` | `folded into tool schema handling (validation.rs); StringEnum is a schema-shape helper` | done |
| `src/utils/uuid.ts` | `src/ai/utils/uuid.rs` | done |
| `src/utils/validation.ts` | `src/ai/utils/validation.rs` | done |

### Tests

| TypeScript test | Rust test | Status |
|---|---|---|
| `test/abort.test.ts` | `tests/ai_live_abort.rs` | live (all 41 cases use the TS provider/model matrix and credential gates) |
| `test/anthropic-adaptive-thinking-models.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-auth-token.test.ts` | `tests/ai_anthropic_auth_token.rs` | done (SDK-mock cases assert on the captured HTTP request instead of SDK constructor options) |
| `test/anthropic-cache-write-1h-cost.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-eager-tool-input-compat.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-eager-tool-input-e2e.test.ts` | `tests/ai_live_anthropic_features.rs` | live (per-provider probes use the TS env/OAuth gates) |
| `test/anthropic-empty-thinking-signature-compat.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-force-adaptive-thinking.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-long-cache-retention-e2e.test.ts` | `tests/ai_live_anthropic_features.rs` | live (per-provider probes use the TS env/OAuth gates) |
| `test/anthropic-oauth.test.ts` | `tests/ai_oauth_anthropic.rs` | done |
| `test/anthropic-opus-4-8-smoke.test.ts` | `tests/ai_live_anthropic_features.rs` | live (gated on ANTHROPIC_API_KEY) |
| `test/anthropic-sse-parsing.test.ts` | `tests/ai_anthropic_stream.rs` | done |
| `test/anthropic-temperature-compat.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/anthropic-thinking-disable.test.ts` | `tests/ai_anthropic_payload.rs` + `tests/ai_live_thinking.rs` | done (offline cases plus the gated E2E entry) |
| `test/anthropic-tool-name-normalization.test.ts` | `tests/ai_anthropic_payload.rs` + `tests/ai_live_anthropic_features.rs` | done (offline mapping plus the Anthropic OAuth-gated E2E entry) |
| `test/azure-openai-base-url.test.ts` | `tests/ai_azure_responses.rs` | done |
| `test/azure-openai-responses-reasoning-replay.test.ts` | `tests/ai_azure_responses.rs` | done |
| `test/azure-openai-tool-choice.test.ts` | `tests/ai_azure_responses.rs` | done |
| `test/azure-utils.ts` | `tests/ai_azure_responses.rs` + `tests/common/live.rs` | done |
| `test/baseten-models.test.ts` | `tests/ai_model_catalogs.rs` | done (catalog cases; the chat_template_args payload cases live in `tests/ai_openai_completions.rs`) |
| `test/bedrock-convert-messages.test.ts` | `tests/ai_bedrock_stream.rs` | done (the two unknown-content-block cases are N/A: Rust's closed content enums cannot carry unknown block types) |
| `test/bedrock-credentials.test.ts` | inline in `src/ai/api/bedrock_converse_stream.rs` | done (observed on the resolved dispatch config) |
| `test/bedrock-custom-headers.test.ts` | inline in `src/ai/api/bedrock_converse_stream.rs` | done (middleware apply behavior observed on the outgoing header list; the SDK step/priority/name registration mechanics are N/A) |
| `test/bedrock-endpoint-resolution.test.ts` | inline in `src/ai/api/bedrock_converse_stream.rs` | done (observed on the resolved dispatch config instead of the SDK constructor) |
| `test/bedrock-error-metadata.test.ts` | `tests/ai_bedrock_stream.rs` | done |
| `test/bedrock-models.test.ts` | `tests/ai_bedrock_stream.rs` + `tests/ai_live_bedrock.rs` | done (offline cases plus the gated model matrix) |
| `test/bedrock-raw-stop-reason.test.ts` | `tests/ai_bedrock_stream.rs` | done |
| `test/bedrock-redacted-reasoning.test.ts` | `tests/ai_bedrock_stream.rs` | done |
| `test/bedrock-response-headers.test.ts` | `tests/ai_bedrock_stream.rs` | done (local HTTP server) |
| `test/bedrock-thinking-payload.test.ts` | `tests/ai_bedrock_stream.rs` + `tests/ai_live_bedrock.rs` | done (offline payload cases plus the AWS-gated max-token E2E entry) |
| `test/bedrock-utils.ts` | `tests/common/live.rs` | done (credential gate helper) |
| `test/cache-retention.test.ts` | `tests/ai_anthropic_payload.rs` + `tests/ai_openai_completions.rs` + `tests/ai_openai_responses.rs` | done (all three describes offline, env cases via scoped ProviderEnv) |
| `test/cloudflare-gateway-binding.test.ts` | `tests/ai_cloudflare_gateway_binding.rs` | done (init-headers merge, Request-input handling, and abort-signal forwarding; the `signal: null`-clears case is unrepresentable — no `Request`/`init` split) |
| `test/cloudflare-stream.test.ts` | `tests/ai_cloudflare_stream.rs` | done (third case covers the TS `??` placeholder-fallback branch) |
| `test/cloudflare-utils.ts` | `tests/common/live.rs` | done (credential gate helpers) |
| `test/codex-websocket-cached-probe.ts` |  | exception (manual benchmark probe script, not a vitest suite; the websocket transport it measures is ported — see the adapter row — but the probe itself is a live benchmark with no offline assertions to port) |
| `test/compat-env.test.ts` | `tests/ai_compat.rs` | done |
| `test/constrained-sampling.test.ts` | `tests/ai_constrained_sampling.rs` | done |
| `test/context-estimate.test.ts` | `tests/ai_context_estimate.rs` | done |
| `test/context-overflow.test.ts` | `tests/ai_live_overflow.rs` | live (35 cases use the TS provider/model matrix and gates) |
| `test/cross-provider-handoff.test.ts` | `tests/ai_live_cross_provider.rs` | live (gated per fixture using the TS credential resolution) |
| `test/deferred-tools.test.ts` | `tests/ai_deferred_tools.rs` | done |
| `test/empty.test.ts` | `tests/ai_live_empty.rs` | live (120 cases use the TS provider/model matrix and gates) |
| `test/env-api-keys.test.ts` | inline in `src/ai/env_api_keys.rs` | done (scoped env injection) |
| `test/error-body.test.ts` | inline in `src/ai/utils/error_body.rs` | done (representable cases; JS class-instance/pipe-stream/non-Error inputs are unrepresentable — noted N/A in the test module) |
| `test/faux-provider.test.ts` | `tests/ai_faux_provider.rs` | done (all 23 cases through the compat global API; the TS factory throw becomes `FauxResponseStep::Factory` returning `Err`, whose catch now emits the single error event) |
| `test/fetch-option.test.ts` | `tests/ai_anthropic_stream.rs`, `tests/ai_sdk_header_parity.rs`, `tests/ai_openrouter_images.rs`, `tests/ai_mistral.rs`, `tests/ai_codex_stream.rs`, `tests/ai_pi_messages.rs`, `tests/ai_google_stream.rs` | done (all legs exercise injected transports; the Google rejection legs port to `tests/ai_google_stream.rs` — passing the ambient globalThis.fetch explicitly is unrepresentable without an ambient global fetch) |
| `test/fireworks-models.test.ts` | `tests/ai_model_catalogs.rs` + `tests/ai_anthropic_payload.rs` | done (catalog cases in the former, x-session-affinity/cache_control/eager payload cases in the latter) |
| `test/generate-models-strict.test.ts` |  | exception (guards the TS codegen script `scripts/generate-models.ts` itself, same treatment as `test/image-model-data.test.ts`; the generated catalog is committed via `scripts/oracle/export-model-catalog.mts`) |
| `test/github-copilot-anthropic.test.ts` | `tests/ai_anthropic_payload.rs` | done |
| `test/github-copilot-oauth.test.ts` | `tests/ai_oauth_github_copilot.rs` | done (Models getAvailable/store halves and the login-budget case included; the budget case rebuilds the provider with a scripted flow, mirroring the TS global-fetch stub) |
| `test/google-raw-stop-reason.test.ts` | `tests/ai_google_stream.rs` | done |
| `test/google-shared-convert-tools.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-shared-gemini3-unsigned-tool-call.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-shared-image-tool-result-routing.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-shared-retry.test.ts` | `tests/ai_google_stream.rs` | done (via `retry_provider_request`, the port of the shared retry helper) |
| `test/google-shared-signed-empty-blocks.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-thinking-disable.test.ts` | `tests/ai_live_thinking.rs` | live (same Anthropic/Gemini/Vertex/OpenAI/OpenRouter gates) |
| `test/google-thinking-level-map.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-thinking-signature.test.ts` | `tests/ai_google_shared.rs` | done |
| `test/google-vertex-api-key-resolution.test.ts` | `tests/ai_google_vertex.rs` | done (asserted on the resolved dispatch and the public resolvers) |
| `test/image-model-data.test.ts` | | exception — tests the TS oracle generator script (`scripts/generate-image-models.ts`); the generated catalog it produces is committed via `scripts/oracle/export-model-catalog.mts` |
| `test/image-tool-result.test.ts` | `tests/ai_live_tool_calls.rs` | live (42 active cases use the TS matrix; 4 upstream `it.skip` cases remain skipped) |
| `test/images-models.test.ts` | `tests/ai_images_models.rs` (+ `tests/ai_providers.rs` for the builtinImagesModels case) | done |
| `test/images.test.ts` | `tests/ai_live_images.rs` | exception (credential-gated upstream fixture never passes `OPENROUTER_API_KEY` to direct `generateImages`; with the gate enabled both TS and Rust surface `No API key for provider: openrouter` instead of reaching the image API, documented in `tests/ai_live_images.rs`) |
| `test/interleaved-thinking.test.ts` | `tests/ai_live_anthropic_features.rs` | live (gated on Bedrock and Anthropic credentials) |
| `test/kimi-coding-oauth.test.ts` | `tests/ai_oauth_kimi_coding.rs` | done |
| `test/lax-message-content.test.ts` |  | exception (Rust's closed `Message` content types cannot represent null/missing content — the laxness the TS test pins is enforced by the type system; see the same class of note in `tests/harness_truncate.rs`) |
| `test/lazy-module-load.test.ts` | | exception (Node module-registry probe asserting SDK imports stay lazy under bundlers; Rust links statically so there is no lazy loading to observe — see `src/ai/providers/apis.rs`) |
| `test/max-thinking.test.ts` | `tests/ai_models.rs` + `tests/ai_supports_xhigh.rs` + `tests/ai_codex_stream.rs` | done |
| `test/mistral-http-transport.test.ts` | `tests/ai_mistral.rs` | done |
| `test/mistral-raw-stop-reason.test.ts` | `tests/ai_mistral.rs` | done |
| `test/mistral-reasoning-mode.test.ts` | `tests/ai_mistral.rs` | done |
| `test/mistral-tool-schema.test.ts` | `tests/ai_mistral.rs` | done |
| `test/model-catalog-types.test.ts` | `tests/ai_model_catalogs.rs` | done (the runtime Grok-4.5 routing case; the `expectTypeOf` halves are compile-time TS type-level checks with no runtime behavior) |
| `test/model-data-validation.test.ts` | `tests/ai_model_catalogs.rs` | done (ported as embedded-catalog integrity checks — duplicate ids across API groups, group/provider/api consistency, exact generated allowlists; the directory/manifest-hash/script-stamp mechanics validate the TS codegen's shard directory and have no Rust counterpart) |
| `test/models-runtime.test.ts` | `tests/ai_models.rs` | done (+ `tests/ai_auth.rs` and `tests/ai_providers.rs` for the auth-resolution cases) |
| `test/node-http-proxy.test.ts` | inline in `src/ai/utils/node_http_proxy.rs` | done (scoped env; reqwest client construction replaces the undici agent) |
| `test/oauth-auth.test.ts` | `tests/ai_oauth_auth.rs` | done (Models.getAuth lazy-chain cases included; module-barrel introspection is a TypeScript namespace concern) |
| `test/oauth-device-code.test.ts` | `tests/ai_oauth_device_code.rs` | done |
| `test/oauth.ts` | `tests/common/live.rs` | done (auth.json API-key/OAuth resolution and refresh helper) |
| `test/openai-codex-cache-affinity-e2e.test.ts` | `tests/ai_live_cache.rs` | live (gated on the openai-codex OAuth token) |
| `test/openai-codex-oauth.test.ts` | `tests/ai_oauth_openai_codex.rs` | done |
| `test/openai-codex-stream.test.ts` | `tests/ai_codex_stream.rs` | done (SSE, websocket, and zstd cases; the websocket tests inject mock sockets through the WebSocket factory and serialize on a shared lock, real 50ms windows replace `vi.useFakeTimers`, and the age-limit case overrides the cache clock in place of `vi.setSystemTime`) |
| `test/openai-completions-cache-control-format.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/openai-completions-empty-tools.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/openai-completions-prompt-cache.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/openai-completions-raw-stop-reason.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/openai-completions-reasoning-details.test.ts` | `tests/ai_openai_completions_replay.rs` | done |
| `test/openai-completions-response-model.test.ts` | `tests/ai_openai_completions_replay.rs` | done |
| `test/openai-completions-retry.test.ts` | `tests/ai_openai_completions_replay.rs` | done (+ `tests/ai_retry.rs` for the shared provider-retry helper cases) |
| `test/openai-completions-thinking-as-text.test.ts` | `tests/ai_openai_completions_replay.rs` | done |
| `test/openai-completions-thinking-token-budget.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/openai-completions-tool-choice.test.ts` | `tests/ai_openai_completions.rs` + `tests/ai_openai_completions_replay.rs` | done (payload/options cases in the former, stream/replay cases in the latter) |
| `test/openai-completions-tool-result-images.test.ts` | `tests/ai_openai_completions_replay.rs` | done |
| `test/openai-responses-cache-affinity-e2e.test.ts` | `tests/ai_live_cache.rs` | live (gated on OPENAI_API_KEY) |
| `test/openai-responses-compat.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-empty-tool-result.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-foreign-toolcall-id.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-message-id.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-namespace.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-partial-json-cleanup.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-reasoning-replay-e2e.test.ts` | `tests/ai_live_reasoning_replay.rs` | live (gated on OPENAI_API_KEY and ANTHROPIC_API_KEY) |
| `test/openai-responses-terminal-event.test.ts` | `tests/ai_openai_responses.rs` | done |
| `test/openai-responses-tool-result-images.test.ts` | `tests/ai_live_responses_tools.rs` | live (same OpenAI/Azure/Copilot/Codex gates) |
| `test/openrouter-cache-control-models.test.ts` | `tests/ai_model_catalogs.rs` | done |
| `test/openrouter-cache-write-repro.test.ts` | `tests/ai_live_cache.rs` | live (gated on OPENROUTER_API_KEY) |
| `test/openrouter-images.test.ts` | `tests/ai_openrouter_images.rs` | done (mock at the `HttpFetch` transport replaces the OpenAI-SDK mock) |
| `test/openrouter-oauth.test.ts` | `tests/ai_oauth_openrouter.rs` | done |
| `test/openrouter-reasoning-options.test.ts` | `tests/ai_openai_completions.rs` | done (the three streamSimple payload cases; the `getOpenRouterThinkingLevelMap` cases test the TS codegen script and follow the `generate-models-strict` exception) |
| `test/overflow.test.ts` | `tests/ai_overflow.rs` | done |
| `test/pi-messages.test.ts` | `tests/ai_pi_messages.rs` | done |
| `test/provider-error-body-passthrough.test.ts` | `tests/ai_openrouter_images.rs` | done (+ the shared normalizer unit tests in `src/ai/utils/error_body.rs`) |
| `test/provider-error-body-regression.test.ts` | `tests/ai_openai_completions.rs` + `tests/ai_openai_responses.rs` + `tests/ai_bedrock_stream.rs` | done (per-tier cases; the TS `$response.body.pipe` stream mechanic has no Rust analog, noted at the bedrock tests) |
| `test/provider-retry.test.ts` | `tests/ai_retry.rs` | done |
| `test/providers.test.ts` | `tests/ai_providers.rs` (+ `tests/ai_models.rs` for the dispatch-error case) | done |
| `test/qwen-token-plan-models.test.ts` | `tests/ai_qwen_token_plan.rs` | done |
| `test/radius-oauth.test.ts` | `tests/ai_oauth_radius.rs` | done |
| `test/reasoning-options.test.ts` |  | exception (tests the TS codegen script `scripts/models-dev-reasoning-options.ts`; its generated output ships through the committed catalog) |
| `test/responseid.test.ts` | `tests/ai_live_responseid.rs` | live (11 cases use the TS provider/model matrix and gates) |
| `test/retry.test.ts` | `tests/ai_retry.rs` | done |
| `test/sampling-options.test.ts` | `tests/ai_openai_completions.rs` | done |
| `test/scratch.ts` | | exception: scratch file, not a test |
| `test/stream.test.ts` | `tests/ai_live_stream.rs` | live (234 cases use the TS provider/model matrix and gates) |
| `test/supports-xhigh.test.ts` | `tests/ai_supports_xhigh.rs` | done |
| `test/telemetry-options.test.ts` | `tests/ai_telemetry_options.rs` | done |
| `test/text.test.ts` | inline in `src/ai/utils/text.rs` | done |
| `test/together-models.test.ts` | `tests/ai_model_catalogs.rs` | done |
| `test/tokens.test.ts` | `tests/ai_live_tokens.rs` | live (26 active cases use the TS matrix; 4 Xiaomi cases remain upstream-skipped) |
| `test/tool-call-id-normalization.test.ts` | `tests/ai_live_tool_call_ids.rs` | live (same Copilot/OpenRouter/Codex credential gates) |
| `test/tool-call-without-result.test.ts` | `tests/ai_live_tool_calls.rs` | live (30 cases use the TS provider/model matrix and gates) |
| `test/total-tokens.test.ts` | `tests/ai_live_tokens.rs` | live (35 cases use the TS provider/model matrix and gates) |
| `test/transform-messages-copilot-openai-to-anthropic.test.ts` | `tests/ai_transform_messages.rs` | done |
| `test/unicode-surrogate.test.ts` | `tests/ai_live_surrogates.rs` | live (87 cases use the TS matrix; unpaired surrogates remain unrepresentable in Rust `String`) |
| `test/uuid.test.ts` | inline in `src/ai/utils/uuid.rs` | done |
| `test/validation.test.ts` | `tests/ai_validation.rs` | done (the Function-constructor CSP case has no Rust analog) |
| `test/xai-oauth.test.ts` | `tests/ai_oauth_xai.rs` | done |
| `test/xai-responses.test.ts` | `tests/ai_xai_responses.rs` | done |
| `test/xhigh.test.ts` | `tests/ai_live_thinking.rs` | live (3 cases gated on OPENAI_API_KEY) |
| `test/xiaomi-models.test.ts` | `tests/ai_model_catalogs.rs` | done |
| `test/xiaomi-token-plan-ams-anthropic-empty-signature-smoke.test.ts` | `tests/ai_live_anthropic_features.rs` | live (gated on XIAOMI_TOKEN_PLAN_AMS_API_KEY) |
| `test/zai-coding-plan-models.test.ts` | `tests/ai_model_catalogs.rs` | done |
| `test/zen.test.ts` | `tests/ai_live_zen.rs` | live (per-model smoke cases gated on OPENCODE_API_KEY) |

### Documented deviations and deferrals

Deviation audit (M1): each entry below is a language/runtime difference the
port keeps. No `done` row carries a hidden gap, and no implementable gap remains
tracked.

Kept language/runtime differences:

- `src/bun-oauth.ts` (Bun credential export) is a runtime entry point for
  the Bun environment; its Bun-specific export has no Rust counterpart.
  `src/cli.ts` has a Rust equivalent (`src/ai/cli.rs` + `src/bin/pi-ai.rs`)
  with the same help/list/login surface; see its row above.
- `src/compat.ts` and `src/legacy-api-aliases.ts` are re-export shims over
  the API implementations; they land with the providers.
- `index.ts` public re-exports are mirrored as they land module by module.
- The lazy module adapters collapse to direct dispatch: Rust links
  statically, so the Node module-registry probe in
  `test/lazy-module-load.test.ts` and the Bun `setBedrockProviderModule`
  override have nothing to observe.

Closed implementable gaps:

- Credential-gated TypeScript suites have env-gated Rust live-test entries
  with the same provider/model matrices.
- `auth.json` uses the platform default file mode, matching TypeScript's
  `writeFileSync` call without an explicit mode.
- Proxy URLs preserve TypeScript `URL` trailing-slash normalization.
- Google adapters reject custom fetch with the TypeScript error message.

### Additional Rust modules

| Rust module | Purpose | Status |
|---|---|---|
| `src/ai/utils/http.rs` | Transport abstraction behind the `fetch` option (`FetchFunction`) | done (trait) |
| `src/ai/utils/reqwest_fetch.rs` | reqwest-backed default transport with node-http-proxy resolution | done |
| `src/ai/utils/sse.rs` | SSE decoder + async event stream (ported from the shared reader in anthropic-messages.ts) | done |

## pi-agent-core

### Source modules

| TypeScript source | Rust module | Status |
|---|---|---|
| `src/agent-loop.ts` | `src/agent/agent_loop.rs` | done |
| `src/agent.ts` | `src/agent/agent.rs` | done |
| `src/harness/agent-harness.ts` | `src/agent/harness/agent_harness.rs` | done (full v2 public contract; unfinished operations explicitly reject with HarnessNotImplemented/HarnessClosed) |
| `src/harness/compaction/branch-summarization.ts` | `src/agent/harness/compaction/branch_summarization.rs` | done |
| `src/harness/compaction/compaction.ts` | `src/agent/harness/compaction/compaction.rs` | done |
| `src/harness/compaction/utils.ts` | `src/agent/harness/compaction/utils.rs` | done |
| `src/harness/env/nodejs.ts` | `src/agent/harness/env/nodejs.rs` | done (tokio::fs/tokio::process rebuild; Windows shell-discovery branches documented in the module) |
| `src/harness/events.ts` | `src/agent/harness/events.rs` | done |
| `src/harness/messages.ts` | `src/agent/harness/messages.rs` | done |
| `src/harness/prompt-templates.ts` | `src/agent/harness/prompt_templates.rs` | done (serde_yaml replaces the yaml package; parse failures surface identically) |
| `src/harness/reducer.ts` | `src/agent/harness/reducer.rs` | done |
| `src/harness/result.ts` | `src/agent/harness/events.rs (module doc)` | exception (std Result replaces the ok/err/isOk/isErr helpers; the TaggedError factory has no Rust counterpart — concrete error structs and match on their codes serve the same purpose) |
| `src/harness/session/context.ts` | `src/agent/harness/session/context.rs` | done |
| `src/harness/session/index.ts` | `src/agent/harness/session/mod.rs` | done |
| `src/harness/session/jsonl.ts` | `src/agent/harness/session/jsonl/mod.rs` | done |
| `src/harness/session/jsonl/codec.ts` | `src/agent/harness/session/jsonl/codec.rs` | done (byte parity pinned by tests/goldens/session-jsonl/ against scripts/oracle/export-session-jsonl.mts) |
| `src/harness/session/jsonl/errors.ts` | `src/agent/harness/session/jsonl/errors.rs` | done |
| `src/harness/session/jsonl/repo.ts` | `src/agent/harness/session/jsonl/repo.rs` | done |
| `src/harness/session/jsonl/storage.ts` | `src/agent/harness/session/jsonl/storage.rs` | done |
| `src/harness/session/jsonl/types.ts` | `src/agent/harness/session/jsonl/types.rs` | done (the structural Pick<FileSystem, ...> becomes the full FileSystem trait object) |
| `src/harness/session/memory.ts` | `src/agent/harness/session/memory.rs` | done |
| `src/harness/session/session.ts` | `src/agent/harness/session/memory.rs (Session)` | done (assertJsonSerializable is enforced by construction; clock seam mirrors Date.now overrides) |
| `src/harness/session/state.ts` | `src/agent/harness/session/state.rs` | done |
| `src/harness/session/testing/conformance.ts` | `src/agent/harness/session/testing/mod.rs` | done (28 of the 30 upstream cases across every group with the TS assertion strength; the two `rejects non-JSON entries/records` cases are unrepresentable — Rust's strongly typed `Entry`/`LaneRecord` cannot carry non-serializable values) |
| `src/harness/session/testing/index.ts` | `src/agent/harness/session/testing/mod.rs` | done |
| `src/harness/session/testing/types.ts` | `src/agent/harness/session/testing/mod.rs` | done |
| `src/harness/session/types.ts` | `src/agent/harness/session/types.rs` | done (the Rust enums remain canonical; TS member names are public aliases with provisioned-entry constructors) |
| `src/harness/skills.ts` | `src/agent/harness/skills.rs` | done (the ignore npm package is replaced by a small gitignore-style matcher covering the loader pattern shapes) |
| `src/harness/system-prompt.ts` | `src/agent/harness/system_prompt.rs` | done |
| `src/harness/telemetry.ts` | `src/agent/harness/telemetry.rs (+ data/telemetry-schemas.json via scripts/oracle/export-agent-telemetry-schemas.mts)` | done (schemas embedded verbatim from the oracle; conditional-type vocabularies remain compile-time-only) |
| `src/harness/tools/bash.ts` | `src/agent/harness/tools/bash.rs` | done |
| `src/harness/tools/edit-diff.ts` | `src/agent/harness/tools/edit_diff.rs` | done (npm diff package replaced by an LCS line diff and unified-patch renderer verified against the package output) |
| `src/harness/tools/edit.ts` | `src/agent/harness/tools/edit.rs` | done |
| `src/harness/tools/file-mutation-queue.ts` | `src/agent/harness/tools/file_mutation_queue.rs` | done (WeakMap keying becomes an Arc-address-keyed registry) |
| `src/harness/tools/image.ts` | `src/agent/harness/tools/image.rs` | done |
| `src/harness/tools/index.ts` | `src/agent/harness/tools/mod.rs` | done (public tool input/detail types and edit replacement helper included) |
| `src/harness/tools/path-utils.ts` | `src/agent/harness/tools/path_utils.rs` | done |
| `src/harness/tools/read.ts` | `src/agent/harness/tools/read.rs` | done |
| `src/harness/tools/tool-context.ts` | `src/agent/harness/tools/tool_context.rs` | done |
| `src/harness/tools/write.ts` | `src/agent/harness/tools/write.rs` | done |
| `src/harness/types.ts` | `src/agent/harness/types.rs` | done |
| `src/harness/utils/shell-output.ts` | `src/agent/harness/utils/shell_output.rs` | done (onChunk receives the progress snapshot computed for that chunk) |
| `src/harness/utils/truncate.ts` | `src/agent/harness/utils/truncate.rs` | done (unpaired-surrogate inputs are unrepresentable in Rust strings; fuzz runs over the valid UTF-8 alphabet) |
| `src/index.ts` | `src/agent/mod.rs` | done |
| `src/node.ts` | `src/agent/node.rs` | done (root agent facade plus `NodeExecutionEnv`) |
| `src/proxy.ts` | `src/agent/proxy.rs` | done (request runs through the crate `HttpFetch` transport, injectable via `ProxyStreamOptions.fetch`, instead of `globalThis.fetch`; cancellation is observed between body chunks) |
| `src/search/index.ts` | `src/agent/search/mod.rs` | done |
| `src/search/scanning.ts` | `src/agent/search/mod.rs` | done |
| `src/stream-fn.ts` | `src/agent/stream_fn.rs` | done |
| `src/types.ts` | `src/agent/types.rs` (+ `src/ai/types.rs` for the shared LLM types) | done |

### Documented deviations (agent core)

- `StreamFn` returns `Result<AssistantMessageEventStream, String>`: the
  Rust analog of a throwing TypeScript stream function. Contract-violating
  failures escape the loop as `Err` exactly like uncaught throws, driving
  the Agent failure-event sequence (`agent.test.ts` "thrown run failures").
- Hook callbacks (`convertToLlm`, `transformContext`, `getApiKey`,
  `shouldStopAfterTurn`, `prepareNextTurn`, steering/follow-up getters)
  are infallible, matching the TypeScript callback types. `beforeToolCall`
  mutates the shared validated-arguments value through an
  `Arc<Mutex<serde_json::Value>>`, preserving the mutate-without-
  revalidation behavior asserted upstream.
- `Agent` exposes state through accessor methods (`system_prompt()`,
  `set_messages(...)`, ...) instead of the mutable `agent.state` object;
  `prompt()`/`continue_()`/`reset()` return `Result` where TypeScript
  throws. `Agent` methods take `&self` — share it via `Arc` across tasks.
- Custom application messages (TypeScript interface merging on
  `CustomAgentMessages`) ride as `AgentMessage::Custom` carrying the role
  plus the full JSON payload.
- `AgentTool.execute` receives the validated arguments as JSON and returns
  `Result<AgentToolResult, String>` (throw → error tool result); tool
  update callbacks queue their events and settle before
  `tool_execution_end`, preserving the settle-then-ignore semantics.

### Tests

| TypeScript test | Rust test | Status |
|---|---|---|
| `test/agent-loop.test.ts` | `tests/agent_loop.rs` | done |
| `test/agent.test.ts` | `tests/agent.rs` | done |
| `test/e2e.test.ts` | `tests/agent_e2e.rs` | done (`test/utils/calculate.ts` ports with a minimal arithmetic evaluator standing in for `new Function` eval — same grammar the suite exercises) |
| `test/proxy.test.ts` | `tests/agent_proxy.rs` | done (mock injected through `ProxyStreamOptions.fetch` instead of a `vi.stubGlobal` fetch stub) |

### Harness tests

| TypeScript test | Rust test | Status |
|---|---|---|
| `test/harness/agent-harness-scaffold.test.ts` | `tests/harness_agent_harness.rs` | done |
| `test/harness/branch-summarization.test.ts` | `tests/harness_compaction.rs` | done |
| `test/harness/compaction.test.ts` | `tests/harness_compaction.rs` | done (offline preparation/cut/token suites plus the full faux-provider summary suites: reasoning pass-through, prompt inclusion of previous summaries/custom instructions, string-result preservation, failed/aborted error results, maxTokens clamping, error-without-throwing, split-turn usage combining, turn-prefix reasoning/errors, the split-turn prior-file-operations preparation, and the result-with-details case; the combines-usage case ports the TS `completeSimple` stub as a scripted `ProviderStreams` provider because the Rust faux always estimates usage like the TS faux) |
| `test/harness/events.test.ts` | `tests/harness_events.rs` | done |
| `test/harness/nodejs-env.test.ts` | `tests/harness_nodejs_env.rs` | done (platform-faking WSL test and win32-only skipIf cases are documented exceptions) |
| `test/harness/prompt-templates.test.ts` | `tests/harness_resources.rs` | done (loading, substitution, diagnostics, and symlinked-file cases plus the sourced source-info and sourced-diagnostics cases over `load_sourced_prompt_templates`) |
| `test/harness/reducer.test.ts` | `tests/harness_reducer.rs` | done (corruption taxonomy, reduction shapes, tool batches, the overflow guard, committed operation-owned configuration after the anchor, bounded-recovery input immutability, deferred-write tool-batch non-resolution, unfulfilled result ids from earlier attempts, and the determinism/no-alias cases; the Object.freeze checks become clone-compare assertions) |
| `test/harness/resource-formatting.test.ts` | `tests/harness_resources.rs` | done |
| `test/harness/session/context.test.ts` | `tests/harness_session.rs` | done |
| `test/harness/session/jsonl.test.ts` | `tests/harness_session_jsonl.rs` + `tests/harness_session_conformance.rs` | done (backend-specific cases plus the full conformance matrix) |
| `test/harness/session/jsonl-codec.test.ts` | `tests/harness_session_jsonl.rs` | done |
| `test/harness/session/jsonl-storage.test.ts` | `tests/harness_session_jsonl.rs` | done (storage-level repair and fork cases through the NodeExecutionEnv filesystem) |
| `test/harness/session/memory.test.ts` | `tests/harness_session.rs` + `tests/harness_session_conformance.rs` | done (backend-specific cases plus the full conformance matrix) |
| `test/harness/session/search.test.ts` | `tests/harness_resources.rs` | done (in-memory projected-source scanning, entry-type filters with abort signals, label projections, and the JSONL-from-disk scanning source) |
| `test/harness/skills.test.ts` | `tests/harness_resources.rs` | done (all six cases with the sourced-skills source-info case asserting the full struct: name, description, content, `filePath`, disableModelInvocation, and the preserved source) |
| `test/harness/system-prompt.test.ts` | `tests/harness_resources.rs` | done |
| `test/harness/telemetry.test.ts` | `tests/harness_telemetry.rs` | done (the docs-regeneration case checks the same oracle-rendered payload) |
| `test/harness/tools.test.ts` | `tests/harness_tools.rs` | done (read/write/edit/bash suites incl. image detection, stub-env late-output, the injected image processor delegation, the mutation-queue lock held until aborted writes/edits settle, edits through symlinks, and bash update-coalescing with truncated full-output persistence) |
| `test/harness/truncate.test.ts` | `tests/harness_truncate.rs` | done |
| `test/harness/session-test-utils.ts` | `tests/common/mod.rs` | done (afterEach cleanup becomes RAII) |
