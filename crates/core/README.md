<div align="center">

# pi-core-rs

**Holt's Rust runtime for the agent, AI, and telemetry core**

<p>
  <code>Rust 2024</code>&nbsp;&nbsp;·&nbsp;&nbsp;
  <code>v0.84.4 compatibility target</code>&nbsp;&nbsp;·&nbsp;&nbsp;
  <code>No Node.js runtime</code>
</p>

</div>

`pi-core-rs` is a Rust port of `@earendil-works/pi-agent-core`, `pi-ai`, and
`pi-telemetry` v0.84.4. It provides the same core building blocks as a native
Rust library, plus a small `pi-ai` command-line binary for OAuth credentials.

> **Project status — Holt-owned, actively maintained**
>
> Originally a standalone port developed against the upstream TypeScript
> sources as a behavioral oracle; now a Holt workspace crate (ADR-0043).
> Upstream pi is no longer tracked — compatibility tests and
> serialized-output goldens keep the v0.84.4 behavior pinned.

## What it provides

| Layer | Capabilities | Rust entry point |
| --- | --- | --- |
| **Telemetry** | Nested spans, attributes, events, status, typed schemas, in-memory and no-op backends | `pi_core::telemetry` |
| **AI** | Provider-neutral streaming, normalized events, tool calls, images, reasoning, usage and cost accounting | `pi_core::ai` |
| **Authentication** | Environment-based API keys, credential stores, OAuth flows, provider-specific auth resolution | `pi_core::ai::auth` |
| **Agent** | Async prompt loop, mutable state, lifecycle events, tool execution, steering, follow-ups, cancellation, and continuation | `pi_core::agent::agent::Agent` |
| **Harness** | Memory and JSONL sessions, branches and lanes, reduction, compaction, summaries, skills, templates, search, and filesystem tools | `pi_core::agent::harness` |
| **CLI** | List OAuth providers and save OAuth credentials to `auth.json` | `pi-ai` |

The crate is designed to run without Node.js at build time or runtime.

## AI provider support

The provider layer includes adapters for:

- Anthropic Messages
- OpenAI Chat Completions, Responses, and Codex Responses
- Azure OpenAI Responses
- Google Generative AI and Google Vertex AI
- Mistral Conversations
- Amazon Bedrock Converse Stream
- Pi Messages
- Cloudflare AI Gateway and Workers AI
- OpenRouter image generation
- Radius and a broad set of OpenAI-compatible providers, including DeepSeek,
  Groq, Cerebras, Together, Fireworks, NVIDIA, Moonshot, MiniMax, Xiaomi,
  Qwen, Z.AI, OpenCode, Hugging Face, and others

Provider features include SSE normalization, partial tool-argument handling,
thinking blocks, stop reasons, retries, cancellation, context overflow
handling, deferred responses, prompt-cache metadata, and usage accounting.

## Quick start

### Requirements

- Rust 1.88 or newer
- Network access only for live provider requests or OAuth flows

### Build

From the repository root:

```bash
cargo build
```

## Use it as a library

For a Holt workspace consumer, add the crate as a path dependency. The package
is named `pi-core-rs`, while the Rust crate is imported as `pi_core`.

```toml
[dependencies]
pi-core-rs = { path = "crates/core" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The smallest deterministic agent uses the built-in faux provider, so it needs
no credentials or network access:

```rust
use std::sync::Arc;

use pi_core::agent::agent::{Agent, AgentOptions};
use pi_core::ai::compat::{register_faux_provider, stream_simple};
use pi_core::ai::providers::faux::{
    faux_assistant_message, FauxMessageOptions, FauxResponseStep,
    RegisterFauxProviderOptions,
};

#[tokio::main]
async fn main() -> Result<(), String> {
    let faux = register_faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("Hello from pi-core-rs", FauxMessageOptions::default()),
    ))]);

    let agent = Agent::new(AgentOptions {
        stream_fn: Some(Arc::new(|model, context, options| {
            Ok(stream_simple(model, context, options))
        })),
        ..Default::default()
    });

    agent.set_model(faux.get_model());
    agent.prompt("Say hello").await?;
    println!("messages: {}", agent.messages().len());
    Ok(())
}
```

For a real provider, replace the faux stream with a `Models` or compatibility
stream and select a model from the generated catalog:

```rust
use pi_core::ai::providers::builtin::get_builtin_models;

let model = get_builtin_models("anthropic")
    .into_iter()
    .next()
    .expect("the provider catalog contains at least one model");
```

## Use API keys or OAuth

API-key providers read their configured environment variables. Common examples
are:

```bash
export ANTHROPIC_API_KEY="..."
# or:
export OPENAI_API_KEY="..."
export GEMINI_API_KEY="..."
```

The provider registry contains the complete provider-to-environment-variable
mapping. API keys can also be supplied through the Rust request/auth types.

The `pi-ai` binary handles the built-in OAuth flows:

```bash
# Show the available OAuth commands and providers
cargo run --bin pi-ai -- help
cargo run --bin pi-ai -- list

# Start an interactive OAuth flow
cargo run --bin pi-ai -- login openai-codex
```

OAuth credentials are merged into `auth.json` in the current working
directory. Keep that file out of version control. The exact provider list is
available from `pi-ai list`; it currently includes Anthropic, GitHub Copilot,
Kimi For Coding, OpenAI Codex, OpenRouter, Radius, and xAI.

## Sessions, tools, and compaction

The harness layer exposes the lower-level pieces needed to build a coding or
workflow agent:

- memory-backed sessions for fast tests and ephemeral runs;
- v4 JSONL session storage with branching, lanes, metadata, and usage records;
- deterministic state reduction and record-log validation;
- context-token estimation, compaction preparation, and branch summaries;
- `bash`, `read`, `write`, and `edit` tools over a Node-compatible execution
  environment;
- prompt-template and skill loaders, resource formatting, and session search;
- telemetry schemas and helpers for AI requests and harness operations.

The high-level `AgentHarness` v2 contract is present. Operations that are still
scaffolded by the ported v0.84.4 contract intentionally return explicit
`HarnessNotImplemented` or `HarnessClosed` errors instead of pretending to be
available. The lower-level session, tool, compaction, reducer, and resource
modules are independently exposed and tested.

## Project layout

```text
src/
├── telemetry/                 # pi-telemetry port
├── ai/                        # providers, streaming, models, auth, images
├── agent/                     # agent loop and harness
└── bin/pi-ai.rs               # OAuth helper binary
tests/all/                     # single integration target: parity,
                               # conformance, and live-gated suites
tests/goldens/                 # serialized-output fixtures from the port
```

## Verification

Run the focused test suites while developing, then the full checks from the
workspace root:

```bash
cargo test -p pi-core-rs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

The default run is deterministic: live API calls, OAuth credential reads or
refreshes, and local model servers are skipped and stay visible as skipped
cases. Use `PI_TEST_LIVE=1 cargo test -p pi-core-rs` to exercise them with
configured credentials.

## License

MIT
