# Terminals

User-operated PTYs the app owns: each terminal belongs to a chat (or, chat-less, to the terminal panel itself), lives only as long as the app does, and never touches the agent. This page covers session lifecycle, bounded replay, and the close-confirmation path.

Not here: the agent's `bash` tool (`engine::tools` — a separate execution path, separately gated by the permission modes) and the window/dock chrome the terminal panel sits in (`ui::shell`).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Sessions, replay, exit | `Terminals` owns the registry; each `Session` holds a PTY pair, a reader thread, an exit watcher, and the replay buffer | [`crates/engine/src/terminals.rs`](../../crates/engine/src/terminals.rs) |
| RPC surface | `OpenTerminal`, `WriteTerminal`, `ResizeTerminal`, `SubscribeTerminal`, `CloseTerminal`, `CloseAllTerminals`, `ListTerminals`; typed params in `holt_rpc::terminals` | [`crates/engine/src/rpc/terminals.rs`](../../crates/engine/src/rpc/terminals.rs), [`crates/rpc/src/terminals.rs`](../../crates/rpc/src/terminals.rs) |
| Root resolution | A known chat's working directory wins (the workspace resolution, errors included); a chat-less terminal takes the caller's explicit `cwd`, else the user's home | `open_terminal` / `chatless_terminal_root` in `rpc/terminals.rs` |
| Emulator | `alacritty_terminal` 0.26 `Term` + vte ANSI parsing, 10,000 scrollback lines, search inside scrollback | [`crates/ui/src/terminal/emulator.rs`](../../crates/ui/src/terminal/emulator.rs) |
| Panes and tabs | Split panes (⌘D / ⇧⌘D); tabs on the embedded right-pane panel — the bottom dock stays chat-only; an embedded panel with no chat selected scopes its tabs to the Space | [`crates/ui/src/terminal/panel.rs`](../../crates/ui/src/terminal/panel.rs), [`pane.rs`](../../crates/ui/src/terminal/pane.rs) |
| Close confirmation | One path for window chrome, ⌘Q, and native Dock quit (`on_should_quit`): unsaved file drafts decide first, then running terminals | [`crates/ui/src/terminal/lifecycle.rs`](../../crates/ui/src/terminal/lifecycle.rs) |

## Runtime path

1. **Open.** `OpenTerminal {chatId, cwd?, cols, rows}`. A known chat's terminal roots at the chat's working directory — the same resolution `SearchFiles` uses, its errors included. A chat-less one (the new-chat canvas keys its tabs by Space; the no-project empty state by nothing) takes the explicit `cwd`, else the user's home; an empty override means "not given". The directory must exist and sizes must be 1–1000 cells per axis. It spawns the user's login shell (`-l -i` for POSIX shells; none for cmd.exe, the Windows fallback) with `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=Holt`.
2. **Output.** A dedicated reader thread reads the PTY (16 KiB chunks) and pushes base64 `Data` events with monotonic seqs into the session's replay, waking subscribers. `SubscribeTerminal {terminalId, afterSeq}` streams the replay from `afterSeq`, then follows live.
3. **Exit.** On EOF the reader drains, polls the child (10 ms) for its exit code, and only then publishes `Exit {exitCode}` — after the final output, never before it. A second watcher thread polls `try_wait` (20 ms) for shells killed from outside; whichever observes first wins, and `finish` is idempotent. The stream ends once `Exit` has been delivered.
4. **Input and size.** `WriteTerminal` caps one chunk at 256 KiB (checked before decode) and refuses an exited session ("terminal has exited"); `ResizeTerminal` revalidates 1–1000.
5. **Close.** `CloseTerminal` ends one session; `CloseAllTerminals` ends every one (window close); chat deletion ends a chat's sessions and records the chat as closed, so a later open for it fails ("terminal owner is closed") — a delete-then-open race cannot resurrect a terminal. Engine drop closes everything.

## Constraints, state, and failure behavior

- **Nothing persists.** Sessions are memory only; there is no background host and no restoration across app restarts. A restarted app starts with no terminals.
- **Replay is bounded and honest.** 1 MiB or 4096 events, whichever hits first; evicted output is reported as an explicit `Gap {seq}`. The pane answers a `Gap` by resetting its emulator, printing "[Output was lost while disconnected; this screen is incomplete.]", and continuing the same session — the process is never killed for a replay shortfall.
- **An exited shell keeps its output.** It stays listed (`running: false`) until explicitly closed; writes to it fail.
- **Kill semantics.** Closing kills the PTY's foreground process group and the shell's group (SIGKILL, pids > 1), then reaps. A shell's exit kills the other processes in its login session — once, under the child lock — so background jobs with redirected output do not outlive the terminal.
- **Confirmation counts jobs, not shells.** `ListTerminals` reports `running` (the shell lives) and `hasRunningJobs` (some other process in the shell's session lives; an idle shell is not a job). The close/quit prompt counts only `hasRunningJobs`; the session scan is `getsid`-based (`proc_listallpids` on macOS, `/proc` on Linux) and unix-only — on Windows `hasRunningJobs` is always false and close needs no confirmation.
- **Failure refuses, never half-closes.** If listing fails or `CloseAllTerminals` errors, the confirmation path reports the error and the app stays open — nothing was terminated.
- **No agent interaction.** Terminals never enter the run loop: no permission gate, no History, no transcript. The user operates them; the agent's `bash` tool is a different execution path.

## Verification

```sh
cargo test -p holt-engine terminal
cargo test -p holt-ui terminal
```

The engine suite (`crates/engine/src/terminals.rs`) runs real PTYs: input/resize/output/exit/replay ordering, chat close ending a running process and refusing a late open, idle-shell vs running-job status, background jobs not surviving shell exit, replay eviction producing a `Gap`, and rejection of bad sizes, directories, and input. Handler tests cover chat-less root resolution. The UI suite covers the emulator and `Gap` handling.
