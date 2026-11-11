# Background processes

Agent-spawned long-running commands, tracked as chat-owned process groups: `spawn_background` starts one detached from its Turn, the RPC surface lists and watches it, and the user (or the agent) ends it. This page covers the registry, the toolset, the RPC surface, and the lifecycle every spawned group follows. The UI is a titlebar entry (left of the terminal toggle, hidden when the chat owns no process): an icon with the running-count badge whose popover lists the processes — stop/remove per row, and the selected row's newest log lines beneath.

Not here: the user's PTY terminals (see [terminals](terminals.md) — a separate subsystem with its own registry; a background process is pipes-only and never interactive), and the foreground `bash` tool (see [toolset-mechanics](toolset-mechanics.md); background spawns share its shell and process-group conventions but never its wait).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Registry | `BackgroundProcesses` owns one `Process` per spawn: pipes, a bounded output log, group lifetime, and exit notices queued per chat | [`crates/engine/src/background.rs`](../../crates/engine/src/background.rs) |
| Bounded log | The shared 1 MiB / 4096-event replay with honest `Gap` eviction, reused from the terminals subsystem | [`crates/engine/src/output_log.rs`](../../crates/engine/src/output_log.rs) |
| Agent tools | `spawn_background` (gated, like `bash`), `process_status` (one-shot snapshot + tail; description forbids polling), `kill_background_process` — mounted for every Turn that keeps the agent shell, attributed to the owning chat (subagent spawns land on the parent) | [`crates/engine/src/tools/background.rs`](../../crates/engine/src/tools/background.rs) |
| RPC surface | `ListBackgroundProcesses`, `WatchBackgroundProcesses` (snapshot + every change), `SubscribeBackgroundProcess` (the `TerminalEvent` log stream), `KillBackgroundProcess`, `RemoveBackgroundProcess`, `ClearExitedBackgroundProcesses`; typed params in `holt_rpc::background`. The list/watch surface is MERGED (issue #77): each row also carries a `kind`, and live background workflow runs ride it beside the processes — a workflow row has no pid/exit (it leaves the list when the run settles; the transcript chip carries the outcome), shows `agents`/`tokens` progress, and stops through `StopWorkflow` with `processId` as the run's call id — never through this surface's kill. The agent's own `spawn_background`/`process_status` tools see processes only | [`crates/engine/src/rpc/background.rs`](../../crates/engine/src/rpc/background.rs), [`crates/rpc/src/background.rs`](../../crates/rpc/src/background.rs), [`crates/engine/src/workflow/mod.rs`](../../crates/engine/src/workflow/mod.rs) (`Workflows::list`) |
| Gate & grants | `spawn_background` is a mutating name; an always-allow grant matches by command prefix exactly like a `bash` grant | [`crates/engine/src/gate.rs`](../../crates/engine/src/gate.rs) |
| Titlebar entry | The icon button (badge = running count — processes and live workflow runs alike, persistent on a real chat, glass-hover while its popover is open — same parity as the terminal/right-pane toggles) and its narrow popover: one raised two-line block per row — command (or run name) over `pid · time` (or `N agents · tokens · time`) — with stop/remove; a workflow row's stop is the armed two-click kind, ending in `StopWorkflow`; no log area | [`crates/ui/src/shell/tabs.rs`](../../crates/ui/src/shell/tabs.rs) |
| Completion notices | A group's exit queues `{command, exitCode, tail}` on its chat; the next main-chat Turn appends them to the system prompt — only when the queue is non-empty, so an uneventful Turn never re-keys the provider's cached prefix | `agent.rs` (`take_exit_notices`), `background.rs` |

## Runtime path

1. **Spawn.** `spawn_background {command}` passes the permission gate (confirm-changes / auto-review like `bash`), then the registry spawns the command through the same shell, flags, and process-group conventions as the foreground `bash` tool — pipes (no PTY), `stdin` null, own group (unix) / Job Object (windows). The tool returns immediately with the process id. Unix records the group under `shell-groups/` while it runs, so a hard kill is swept at the next launch — the same reaping the foreground tool uses. (A workflow run joins the same list when it launches — see workflows — and leaves it when it settles; its bumps wake the same watch.)
2. **Follow.** Two pipe readers append into the process's bounded log; `SubscribeBackgroundProcess {processId, afterSeq}` replays from `afterSeq` then follows live, ending at the `Exit` event. `WatchBackgroundProcesses` pushes the whole list snapshot on every structural change.
3. **Exit.** The supervisor waits the shell leader, then polls until the whole group is gone — `cmd &` leaves a leader-less group that is still running, and its members still hold the pipes. On finish it publishes `Exit {exitCode}`, forgets the shell-groups record, and queues the chat's exit notice. The entry stays listed (`running: false`) with its code and tail until the user clears it.
4. **Kill.** `KillBackgroundProcess` (the agent's tool, or a future panel's stop button) posts SIGTERM to the group, escalates to SIGKILL after 250 ms (windows: closes the Job Object). The supervisor observes the exit and follows path 3 — kill never removes the row.
5. **Teardown.** Deleting a chat kills and de-lists its processes, drops its notices, and refuses later spawns ("owner is closed"); engine shutdown kills everything and stops the registry. Nothing survives a restart.

## Constraints, state, and failure behavior

- **Detached from the Turn.** Once spawned, a process ignores the Turn's cancellation: interrupt, Turn end, and later Turns never kill it (ADR-background-processes-are-chat-owned; the majority harness semantics). Death has four causes — the user, the agent, chat deletion, app exit.
- **No caps.** No concurrency limit, no background timeout, no idle reaping; the 1 MiB-per-process log bounds memory.
- **The notice is delivered once.** `take_exit_notices` drains at the next main-chat Turn start; subagent Turns never consume notices.
- **Read-only by design.** No stdin path exists; interactive processes belong in the terminal panel. A running entry cannot be removed by hand (kill first); only exited entries clear (`RemoveBackgroundProcess` / `ClearExitedBackgroundProcesses`).
- **Failure refuses, never half-ends.** An unknown id, an empty command, a closed chat, or a spawn error is a typed error the caller reads; a dead engine (`Drop`) still publishes exits to outstanding subscribers through their `Arc` handles.

## Verification

```sh
cargo test -p holt-engine background
cargo test -p holt-engine output_log
cargo test -p holt-engine tools
cargo test -p holt-engine --test all background_process
```

The engine suite drives real `/bin/sh` groups: output/exit/notice round-trip, kill-keeps-listed, leader-less groups staying running until their last member dies, chat-close killing and refusing late spawns, and bounded-log eviction. The `tests/all` suite runs the whole story through the RPC contract: a scripted Turn spawns via the tool, the list/watch/kill surface answers, the exit notice rides the next Turn's system prompt, and clearing empties the list.
