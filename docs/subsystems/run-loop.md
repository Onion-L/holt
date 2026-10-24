# Run loop

The single-agent Turn execution engine over holt-core: how one admitted run becomes LLM requests and tool rounds, what transport every request rides, how streaming reaches the transcript, and when a Turn ends. This is the machinery every engine-owned model call shares — Turns, subagents, compaction summaries, title tasks, gate reviews.

Not here: what the model is told (see prompts-skills), how work is admitted and paced (see conversation-policy), what audits tool calls (see permission-gate), how events persist (see persistence), and the child-run harness (see subagents).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| The loop | `run_agent_command` drives one Turn: system prompt, the chat's History converted to the LLM view, tool rounds through holt-core's loop, cancellation races throughout. Every call the permission gate lets through is journaled before it runs; a resumed Turn enters through `run_agent_loop_continue`. A Turn ends `Succeeded`, `Failed { reason }`, or `Interrupted` | [`crates/engine/src/agent.rs`](../../crates/engine/src/agent.rs) |
| Transport guard | Every engine-owned request runs behind exactly one guard: the built-in compat stream wrapped with an idle watchdog. Tests inject a scripted provider through `EngineConfig::stream_fn` and get the same watchdog | [`crates/engine/src/stream.rs`](../../crates/engine/src/stream.rs) (`default_stream_fn`, `guard_stream_fn`) |
| Idle watchdog | 300 s bounds the time between events — never total request duration. Five silent minutes means the connection is gone; without it a Turn or child stays `streaming` until an app restart | `stream.rs` (`STREAM_IDLE_TIMEOUT`) |
| Mid-stream retry | A failure arriving after the provider's `Start` but before any content re-dials in place (a small retry budget, seconds of backoff); a failure after content surfaces as-is. Cancellation gives a provider a 1 s window to emit its own terminal abort before the engine synthesizes one | `stream.rs` |
| Request-layer retry | Ten retries on a Turn's critical path (the Turn, its compaction, the gate reviewer) under holt-core's SDK-shaped policy — retryable statuses and network errors, retry-after-aware, backoff capped — riding out roughly a minute of provider overload: the user is waiting and can cancel, so waiting beats failing the Turn | `agent.rs` (`PROVIDER_MAX_RETRIES`) |
| Retry notices | Every scheduled retry fans out through `on_retry` to `WatchTurnRetry` — live-only, fire-and-forget, a lagging consumer is the consumer's failure | `agent.rs` (`on_retry_callback`), [`retry_events.rs`](../../crates/engine/src/retry_events.rs) |
| Streaming to the transcript | Agent events are translated into transcript parts in decode and published on a throttled cadence; session status beats on a heartbeat while work runs | `agent.rs` (`STREAM_PUBLISH_INTERVAL`, `SESSION_HEARTBEAT_INTERVAL`), [`decode.rs`](../../crates/engine/src/decode.rs) |
| Terminal events | `WatchTurnTerminalEvents` carries one typed event per real main-chat Turn, published only after the Turn's transcript, History, and queue completion are durably settled — fire-and-forget, live-only, never able to fail the Turn or delay the queue | [`turn_events.rs`](../../crates/engine/src/turn_events.rs) |
| Overflow detection | A run that ends on a request the provider rejected as too long before producing anything is the one overflow shape a compaction can retry without taking content back — the detection seam conversation-policy's trigger rides on | `agent.rs` (`ends_on_rejected_overflow`) |
| UI cutaways | The streaming entry under the composer; the transient retry chip (a per-second countdown off the notice's `retry_at_ms`, dropped when frames resume); the OS notification on terminal events | [`crates/ui/src/notifications.rs`](../../crates/ui/src/notifications.rs), transcript render |

## Runtime path

1. **Assemble.** The queue driver hands the admitted run its model, reasoning level, credentials snapshot, cancellation token, and the chat's runtime; the loop builds the system prompt and converts the History to the LLM view.
2. **Request.** One guarded transport call per round; retries at the request layer surface as notices, a content-free early failure re-dials in place, silence beyond the watchdog fails the request.
3. **Rounds.** Tool calls resolve through the mounted toolset (toolset-mechanics), events decode into transcript parts, streaming publishes throttled.
4. **End.** The loop settles unresolved tool calls, the Turn ends with one of three outcomes, and the driver settles History, transcript, and queue completion before the terminal event publishes.

## Constraints, state, and failure behavior

- **One watchdog per request, no exceptions** — the wrapper is applied where the transport is created, so an injected test transport cannot bypass it.
- **Events never fail the run**: terminal and retry publishing are fire-and-forget with bounded buffers; a closed or lagging consumer loses frames, never blocks a Turn.
- **Terminal means durable**: the event publishes only after settlement; a completion that cannot be persisted keeps the queue's error and emits nothing.
- **Streams are live-only** — no synthetic initial event, no replay across restart; consumers must tolerate a restart mid-Turn (restart repair is conversation-policy's). A resumed Turn is a fresh run over the same History: it continues from the record rather than from its prompt, and a half-received provider stream is re-requested, never continued.
- A Turn that fails mid-flight keeps an honest record: interrupted Turns keep their partial transcript with synthetic error tool results (see persistence).

## Verification

```sh
cargo test -p holt-engine stream
cargo test -p holt-engine agent
cargo test -p holt-engine --test all turn
```

The suites cover the idle watchdog and cancel-settle window, the mid-stream re-dial and post-content surfacing, retry notice fan-out, the overflow detection shape, event decoding, and whole Turns through `RpcService::handle` against a scripted provider — persistence, repair, and overflow behavior asserted without a real provider.
