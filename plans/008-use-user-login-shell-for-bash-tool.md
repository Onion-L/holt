# Plan 008: Execute agent commands in the user’s login shell

> **Executor instructions**: Follow this plan exactly. This plan is limited to shell selection and environment setup; stop on any STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `b8e5d9a`, 2026-09-12

## Why this matters

The current bash tool always launches `bash -c` and separately probes the user’s login shell only to obtain PATH. This gives GUI-launched Holt a richer PATH, but command semantics can differ from the shell the user actually configured. Running commands through the detected login shell makes PATH and shell behavior consistent while preserving the existing approval boundary.

## Current state

- `crates/engine/src/tools.rs:467` creates `Command::new("bash")` for shell execution.
- `crates/engine/src/tools.rs:629-655` injects `login_shell_path()` into `BashToolOptions`.
- `crates/engine/src/shell_env.rs:32-69` resolves the passwd database login shell and caches a PATH probe; `terminals.rs:108-118` already reuses `login_shell()` for terminal sessions.
- The tool is named `bash` by the pi-core harness, while its description currently says Bash command. If the implementation remains constrained to a fixed tool name, the description must describe the detected login shell instead of promising Bash syntax.

## Commands

| Purpose | Command | Expected |
|---|---|---|
| Engine tests | `rtk cargo test -p holt-engine` | Passes apart from documented pre-existing git-status failures |
| Shell tests | `rtk cargo test -p holt-engine shell_env` | Passes |
| Format | `rtk cargo fmt --all -- --check` | Exit 0 |
| Clippy | `rtk cargo clippy -p holt-engine --all-targets` | Exit 0 |

## Scope

**In scope:** `crates/engine/src/shell_env.rs`, `crates/engine/src/tools.rs`, `crates/engine/src/terminals.rs` only if required for shared shell resolution, and engine shell tests.

**Out of scope:** `alwaysAllow`/gate code, `pi-core-rs` internals unless the existing public configuration cannot select the executable or description, UI changes, and git-status failures.

## Steps

### Step 1: Specify and test shell resolution

Extract a testable resolver that returns an absolute executable path from the passwd database (with `$SHELL` only if the existing project convention requires it), rejects empty/non-executable values, and falls back deterministically to `/bin/sh`. Add hermetic tests for valid zsh/bash, missing value, and invalid path. Do not invoke arbitrary shell strings.

**Verify:** `rtk cargo test -p holt-engine shell_env` passes.

### Step 2: Run bash-tool commands through the resolved login shell

Replace the fixed `bash -c` command construction in the agent execution path with `<resolved-shell> -lc <command>`. Remove the PATH probe and PATH env override once the login shell owns environment construction. Preserve cwd, stdin/stdout/stderr handling, timeout, process-group cleanup, and exit-code behavior.

**Verify:** add/run a hermetic command test proving a login-shell profile variable is visible and `cargo --version` succeeds with a minimal parent PATH.

### Step 3: Align the model-facing tool description

Use the existing description override mechanism if available. Describe the tool as executing a command in the user’s login shell; do not claim Bash-only syntax unless the implementation remains Bash. If the harness cannot override the description or executable, stop and report the required `pi-core-rs` API change instead of silently misdescribing behavior.

**Verify:** inspect the generated tool definition in the existing engine test seam and assert the description contains “login shell” and does not claim fixed Bash execution.

## Test plan

Follow `shell_env.rs` hermetic fake-shell tests and `tools.rs` shell process tests. Cover resolver fallback, login profile loading, minimal inherited PATH, command output/exit status, and description alignment.

## Done criteria

- [ ] Agent commands execute with the resolved login shell using `-lc`.
- [ ] PATH probing and redundant PATH injection are removed or explicitly justified by a testable platform constraint.
- [ ] Invalid shell configuration has deterministic fallback behavior.
- [ ] Tool description matches actual shell semantics.
- [ ] Focused tests, engine clippy, and format checks pass.
- [ ] No gate, UI, or unrelated git-status files are modified.

## STOP conditions

- Existing pi-core APIs cannot select the executable or override the description without an out-of-scope dependency change.
- Login shell startup hangs or consumes application stdin despite bounded, isolated execution.
- Tests show command behavior depends on aliases/functions that cannot be safely loaded in non-interactive `-lc` mode.

## Maintenance notes

Shell startup files are user-controlled and can be slow or noisy; keep timeout and stdio isolation. Review future changes to tool naming, shell invocation flags, and terminal shell resolution together.
