# Windows Compatibility — Traps Inventory and Triage Guide

> Survey date: 2026-10-23, taken on `feat/issue-52-cross-platform` with the
> addressed-path rework in flight. This page is the **platform delta layer**:
> what behaves differently on Windows, where Holt touches it, and which bug
> class a Windows-only symptom most likely belongs to. Behavior contracts and
> per-file policies stay on the [subsystem pages](../subsystems/README.md);
> this page cites code as the entry point for debugging, not as the contract.
> Code references were verified against the working tree on the survey date;
> line numbers drift, so anchor on the function names.

## 1. Read this first — the six recurring root causes

Every Windows-only bug Holt has hit so far reduces to one of six classes.
Classify the symptom into a class before reading any code:

| # | Class | One-line statement | Where it bites Holt |
| --- | --- | --- | --- |
| 1 | Path spelling | Windows has three live spellings of one path (native `\`, addressed `/`, verbatim `\\?\`); string-comparing two of them silently fails | Every boundary that displays, persists, or compares a path |
| 2 | Case sensitivity | NTFS preserves case but matches case-insensitively; Rust `Path` equality is case-sensitive | Grant matching, rename detection, write-refusal checks |
| 3 | Environment shape | `HOME` vs `USERPROFILE`, git-bash's fake `SHELL`, minimal GUI-launch `PATH`, OEM code pages | Shell tool, MCP children, home-dir expansion |
| 4 | Process model | No process groups or signals; `kill_on_drop` reaches only the direct child | bash tool cleanup, terminal jobs, orphaned descendants |
| 5 | FS durability & locking | Directory fsync needs a write handle; rename over an open file fails (sharing violation) | Atomic saves, checkpoint restores, AV/indexer races |
| 6 | Rendering stack | D3D11 driver quirks, Rust-scene↔HLSL struct alignment, DirectWrite family resolution, non-client-area math | gpui_windows, titlebar, fonts, DPI |

Two structural facts amplify all six:

- **CI is macOS-only.** Nothing gates a Windows regression until someone runs
  `cargo test --workspace` on a Windows box. Do that before every merge in
  this port effort; the unix-gated integration tests (see §10) mean the
  Windows suite is also smaller than it looks.
- **The repo has no `.gitattributes`**, and Windows checkouts commonly run
  `core.autocrlf=true` (this survey's box does). Working-tree bytes can be
  CRLF where macOS CI sees LF — any byte-exact fixture or comparison is
  platform-sensitive before the code even runs.

## 2. Class 1 — the path vocabulary

### 2.1 The three spellings

| Spelling | Example | Who produces it | Who consumes it |
| --- | --- | --- | --- |
| Native | `C:\Users\x\repo\src` | the OS (`Path` joins, `read_dir`), `std::fs::canonicalize` (with verbatim prefix) | disk operations |
| Addressed | `C:/Users/x/repo/src`, UNC `//server/share/src` | Holt's normalizers | the model, the UI, persisted records — every human/LLM-facing boundary |
| Verbatim | `\\?\C:\Users\x\repo\src` | `std::fs::canonicalize` **always** on Windows | nobody, on purpose — strip it at the boundary |

The conversion layer lives in
[`crates/engine/src/tools.rs`](../../crates/engine/src/tools.rs):

- `normalize_absolute` — lexical normalization into the addressed vocabulary
  (`\`→`/`, drive letter uppercased, `\\?\` and `\\?\UNC\` unwrapped, `..`
  clamped at the root, bare `C:` completed to `C:/`).
- `strip_verbatim` — unwraps a canonicalized `PathBuf` back to plain
  `C:\…` / `\\server\share\…`.
- `addressed_spelling` — separator fold only.
- `to_absolute` — resolves a tool argument against the cwd. On Windows a
  bare `/x` or `\x` has **no root of its own** (it is drive-relative), so it
  joins the cwd — deliberately, so unix-shaped model output stays inside the
  workspace fence. A full `D:\…` or `\\server\…` keeps its own root.
- `has_windows_root` / `peel_root` — the root grammar the above rely on.

### 2.2 The rule that prevents this class

One vocabulary per boundary: **addressed at every model/UI/persistence
boundary, native at the disk boundary, and never string-compare two paths
that took different routes** — compare as `Path` (separator-agnostic, but
case-sensitive) or through `to_absolute`/`normalize_absolute` first. And
`canonicalize()` output must pass through `strip_verbatim` before it is
displayed, stored, or compared — every raw call site is a latent bug.
Known producers of verbatim strings: `files.rs` (`canonicalize_lenient`),
`git.rs:226-246` (repo discovery — the workdir string reaches the UI in
status payloads), `subagents.rs`, `rpc/workspace.rs`, `rpc/spaces.rs`,
`update.rs`, and `LocalExecutionEnv::new` (now strips it).

Canonicalize call sites that already do it right, as patterns to copy:
`git.rs` `relative_path` (strips verbatim on **both** sides before the
prefix-strip, then folds separators to `/`), `skills.rs` `Catalog::by_path`
(`Path::new(&a) == path`, never string equality).

### 2.3 Windows path facts that produce edge cases

- **Case-insensitive, case-preserving.** `C:/Src/a.rs` and `C:/src/a.rs` are
  the same file; a `Path` comparison says different. Consequences in tree:
  always-allow grants match on exact resolved spelling, so a differently-cased
  re-write re-asks (documented on
  [permission-gate](../subsystems/permission-gate.md)); the write tool's
  symlink-refusal landing check treats a case mismatch as "not the same
  target" and refuses, fail-safe (`tools.rs` `resolve_landing` /
  `write_to_file`); `files.rs` has `same_entry_case_insensitive` for
  case-only renames — the pattern any new case-sensitive comparison should
  reach for.
- **Drive-relative paths.** `\foo` and `foo` resolve against the *current
  drive*, not the cwd's drive. `to_absolute` folds them under the cwd; any
  new code doing its own `Path::join` on model-supplied strings bypasses
  that fence.
- **Reserved device names** (`CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`,
  with extensions: `CON.txt` too) and **illegal filename characters**
  (`< > : " | ? *`), plus trailing dots and spaces, make file creation fail
  with confusing errors. Holt's own ids are uuid-shaped and safe
  (`store.rs` `id_is_path_safe`), but model-chosen filenames reach the write
  tool. Windows Explorer names like `aux.rs` are legal on macOS — a test
  fixture can pass there and fail here.
- **MAX_PATH 260** by default. Verbatim `\\?\` paths bypass it; plain paths
  need the system LongPathsEnabled policy **and** a `longPathAware`
  manifest — the embedded gpui manifest
  (`third_party/gpui/crates/gpui/resources/windows/gpui.manifest.xml`)
  carries DPI awareness but not that. Deep user workspaces + long data-dir
  transcripts are the exposure; uuid ids keep Holt's own files shallow.

### 2.4 Symptom map for this class

| Symptom | First suspects |
| --- | --- |
| Tool refuses a write/read: "outside the workspace" on a path that is inside | `to_absolute` bypass (raw join), verbatim leak, `\`-vs-`/` in a string compare — `tools.rs`, `files.rs` `resolve_inside_root` |
| Skill mention/link doesn't resolve; read of `SKILL.md` renders as a plain read instead of a chip | spelling mismatch between mention side (native) and catalog side (addressed) — `skills.rs` `by_path`, `decode.rs` `skill_read_part` |
| Turn change set empty or paths wrong | `git.rs` `relative_path` — verbatim or case mismatch between tool landing path and workdir |
| Always-allow grant re-asks for the same file | case difference in the resolved spelling — `gate.rs` `GateGrants` |
| UI tree rows lose their git decorations | workdir prefix strip vs row-path spelling — `git.rs` discovery canonicalize |
| Test asserts equality on a canonicalize result and fails on Windows only | verbatim prefix — strip before comparing (`skills.rs` tests' `addressed()` helper is the pattern) |

## 3. Class 5 — filesystem semantics

### 3.1 Rename and atomic replace

- `std::fs::rename` **replaces** an existing destination file on Windows
  (like unix), but **fails** if the destination is open without
  `FILE_SHARE_DELETE` — antivirus, the search indexer, a terminal whose cwd
  is inside a directory being renamed, an editor holding the file. Symptom:
  "Access denied" / "being used by another process" on a save that succeeds
  a second later. The atomic-save sites (`files.rs` `save_file`,
  `write_file_as`; `tools.rs` `replace_atomically`) have no retry — a
  transient AV scan window surfaces to the user as a failed save.
- `files.rs` `rename_noreplace`: the macOS arm uses `renamex_np`; the
  Windows arm uses `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` (a
  native no-replace); the remaining unix arm is a **plain rename**, which
  silently *replaces* a racing destination — anything relying on its
  no-replace guarantee has a Linux-only TOCTOU.
- Deleting a directory that any process (including a Holt terminal) holds as
  cwd fails — relevant to worktree/session cleanup.

### 3.2 Durability (the issue #52 lesson)

Windows's `FlushFileBuffers` refuses a handle opened read-only — a plain
`File::open(dir)?.sync_all()` fails with `ERROR_ACCESS_DENIED`. The fix
pattern is [`store.rs`](../../crates/engine/src/store.rs) `sync_dir`:
open the directory with `write(true)` +
`FILE_FLAG_BACKUP_SEMANTICS`, then `sync_all()`. Any new "flush the parent
so the rename survives a crash" site must use it, not `File::open`. Note
`persist_chats`/`persist_spaces` still write tmp+rename without any fsync —
pre-existing on all platforms, not Windows-specific.

### 3.3 Locks

`engine.lock` uses flock on unix and std `try_lock` (LockFileEx) on Windows
(`instance_lock.rs`) — semantics differ around inheritance by child
processes, which matters only if Holt ever spawns children that outlive it
(see §5: it does, via the shell tool). The log-file lock in
`apps/holt/src/main.rs` is another flock site to re-check on Windows.

### 3.4 Symlinks and permissions

Creating symlinks needs Developer Mode or admin on Windows; git worktrees
clone with `core.symlinks=false` by default, so a repo's symlinks check out
as plain text files. Step checkpoints read workdir bytes and **omit**
`FileMode::Link` entries (`git/checkpoints.rs`) — symlinked repos restore
differently. The append tool's `O_NOFOLLOW`/`ELOOP` TOCTOU backstop is
unix-only (`tools.rs`). The 0600 permission hardening on credential files is
unix-only; on Windows nothing sets ACLs — private-by-default is not
guaranteed there (`credentials.rs`, `mcp/config.rs`,
`provider_settings.rs`, `provider_store.rs`, `web_search_settings.rs`).

### 3.5 File watching

`notify`'s Windows backend is `ReadDirectoryChangesW`: rename arrives as
paired events, a buffer overflow arrives as a "rescan everything" hint (easy
to drop), and recursion semantics ride one handle. Holt's watchers
(`workspace_watch.rs`, `git_status_watch.rs`, `git_watch.rs`,
`turn_change_watch.rs`) already carry poll fallbacks for watcher failure —
but an arming **error** degrades to silence in `workspace_watch.rs`, so "the
file sidebar never updates on Windows" is a watcher-arm failure until proven
otherwise. 200 ms debounces absorb most of the event-shape differences.

### 3.6 Trash

`trash.rs` implements macOS only; off macOS it returns "The system trash is
not available on this platform." Sidebar delete-to-trash is a known-open
Windows gap, not a bug to re-diagnose (Windows would want
`SHFileOperationW`/`IFileOperation`).

## 4. Class 3 — environment and shells

### 4.1 Home directory

Three resolutions exist and they disagree:

- Engine (authoritative): `HOME` → `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH`
  (`local_fs.rs` `home_dir`).
- `apps/holt/src/main.rs` uses deprecated `std::env::home_dir()` for the
  default data dir — reads `USERPROFILE`, but a bare `HOME` expect() once
  panicked at launch (commit 083d4d3's fallout); `HOLT_DATA_DIR` overrides.
- **`crates/core`'s harness resolves the same chain** (`nodejs.rs`
  `home_dir`: HOME → USERPROFILE → HOMEDRIVE+HOMEPATH) — kept aligned with
  the engine's, so `~` expansion agrees across the boundary.

`~`/`~/` expansion accepts the `~\` spelling only on Windows
(`local_fs.rs` `expand_tilde`, `skills.rs` `expand_home`).

### 4.2 Shell selection and arguments

`login_shell()` (`shell_env.rs`): Windows prefers `COMSPEC`, then `$SHELL` —
but a `SHELL` inherited from git-bash names an msys path (`/usr/bin/bash`)
that win32 cannot spawn, so `is_executable_file` filters it (absolute +
regular file; no executable bit on Windows). Fallback: `cmd.exe`.
`command_args()` keeps `-lc` for sh/bash/zsh (git-bash), uses
`/d /s /c` for anything else. The terminal RPC passes `-l -i` only for
POSIX shells — cmd.exe takes no session flags
(`shell_env::interactive_login_args`).

### 4.3 PATH and command resolution

- `login_path()` and `resolve_command()` answer differently per platform:
  `login_path()` probes the login shell on unix and returns `None` on
  Windows by design ("GUI processes get the user PATH from the registry").
  `resolve_command()` walks PATH+PATHEXT on Windows (a bare `npx` is really
  `npx.cmd`, and CreateProcess appends only `.exe`) and resolves via a PATH
  probe on unix.
- The scrubbed-env fallback PATH for shell children is `C:\Windows\System32`
  on Windows (`tools.rs`) — a minimal but valid floor.
- **`.cmd`/`.bat` resolution: `CreateProcess` appends only `.exe`.** An MCP
  stdio server configured as `npx …` (the most common shape) is really
  `npx.cmd`; with `resolve_command` a no-op on Windows, the spawn goes to
  `CreateProcess` as written and fails — "did not initialize within its
  startup timeout" is this failure wearing its disguise. Fix pattern:
  resolve through `where.exe`/PATHEXT at connect time, or spawn through
  `cmd /c`.
- The MCP child env is **not** stripped to nothing: `sanitize_child_env`
  starts from the parent env minus credential-shaped names (case-insensitive
  `TOKEN|SECRET|PASSWORD|KEY|AUTH`), so `SYSTEMROOT`/`COMSPEC` survive
  (`mcp/config.rs`). A subtler edge: Windows env names are
  case-insensitive but the sanitized `HashMap` keys are not — a parent
  carrying `PATH` and `Path` both keeps both entries.

### 4.4 Line endings and code pages

- With `core.autocrlf=true` and no `.gitattributes`, a Windows checkout of
  *this repo* rewrites text files to CRLF (git says so on every `git add`
  here). Tests that compare bytes read from the working tree — checkpoints
  above all, which snapshot exact workdir bytes — see CRLF where CI sees LF.
  The core harness already anticipates the sibling problem
  (`nodejs.rs` `find_bash_on_path` splits on `\r\n`).
- cmd.exe output decodes in the console's **OEM code page** (GBK on zh-CN
  Windows), not UTF-8; bash-tool output passing through
  `String::from_utf8_lossy` turns non-ASCII cmd output into replacement
  characters. Git-bash children emit UTF-8; cmd children do not. A tool
  wrapper that needs correct cmd output must set `CHCP 65001` or decode the
  page explicitly.

## 5. Class 4 — processes, PTYs, job control

- **No process groups on Windows.** The bash tool's `process_group(0)` and
  the whole `shell_groups` orphan registry (record/reap, launch-time kill)
  are `#[cfg(unix)]`; the Windows arm is a Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` around each shell child
  (`tools.rs` `ShellJob`): abort/timeout close the handle and kill every
  surviving descendant, a completed command detaches the job so background
  survivors persist (until Holt exits), and a hard kill of Holt itself
  takes the descendants with it — the OS closes the job handle. The
  shell-groups launch-time reap therefore has no Windows counterpart to
  need.
- The core harness tree-kills on Windows via
  `taskkill /F /T /PID` (`nodejs.rs` `kill_process_tree`).
- **Terminals** go through portable-pty's ConPTY (needs Windows 10 1809+).
  Windows-only gaps: job cleanup and session process enumeration are
  unix-only, so `has_running_jobs` is always false and "close anyway, jobs
  are running" dialogs never appear on Windows (documented on
  [terminals](../subsystems/terminals.md)).
- **Console windows.** `holt.exe` declares
  `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows"]`: the
  release binary opens no console host window, debug builds keep one for
  the `cargo run` dev loop. Every console-subsystem child a GUI parent
  spawns without `CREATE_NO_WINDOW` flashes its own window — all engine and
  core spawns now pass it (`shell_env::CREATE_NO_WINDOW`,
  `nodejs.rs`): the bash tool, MCP stdio children, the `where.exe` probe,
  and `taskkill`.

## 6. git2 on Windows

- **Build**: git2 vendors libgit2 built with `cc` — a fresh Windows box
  needs the MSVC toolchain (`x86_64-pc-windows-msvc` + Visual Studio Build
  Tools) or the build fails before any Holt code compiles.
- **Credentials** (`git.rs` `fetch_all`): one `Cred::default()` attempt
  (system credential helpers — Git Credential Manager on Windows), then
  ssh-agent; never interactive. A private-remote fetch on Windows depends on
  GCM being installed and configured for that host.
- **Status/diff paths**: libgit2 entry paths always use `/` regardless of
  platform; `git.rs` already folds separators at the boundary. The trap is
  the *workdir* side: discovery canonicalizes it (verbatim spelling, §2.2).
- **Identity**: `checkout_identity` hashes `git_dir.display()` verbatim —
  case or spelling variants of the same repo hash to different checkout ids
  (duplicated change-set history after e.g. a case-different re-import).
- **autocrlf / filemode / symlinks**: Holt sets none of them; the user's git
  config decides. On Windows `core.autocrlf=true` checkouts make
  byte-comparisons (checkpoints, turn-change hashes, diff capture) see
  CRLF-vs-index differences that macOS never shows. `core.filemode` and
  `core.symlinks` differences follow the same shape.

## 7. Class 6 — rendering and windowing (gpui_windows)

The vendored platform crate is
`third_party/gpui/crates/gpui_windows/` (D3D11 + HLSL **fxc SM 4.1** — not
D3D12/blade — with DirectComposition for the transparent titlebar,
DirectWrite text, IMM32 IME, and a PerMonitorV2 manifest embedded by
`crates/gpui/build.rs`).

### 7.1 The two fixed rendering bug classes (expect them again)

- **Driver-dependent structured-buffer reads** (commit f4e6689): some
  drivers return zeros when a ranged `StructuredBuffer` SRV spans more than
  a few KB, silently dropping primitives mid-batch. Both instanced draw
  paths (`directx_renderer.rs` `draw_range`,
  `draw_range_with_texture`) now draw in ≤16-instance chunks with per-chunk
  SRV ranges (`create_buffer_view_range`). Symptom was sprites/paths
  vanishing partway down the window on specific GPUs. Any new draw path over
  a structured buffer inherits the chunking rule.
- **Rust-scene ↔ HLSL struct misalignment** (same commit): the HLSL
  `StructuredBuffer` element structs in `shaders.hlsl` must match
  `crates/gpui/src/scene.rs` field-for-field, in order — a field inserted
  only on one side shifts every later member and renders silent garbage, not
  an error. `EdgeFadeParams` was added to *both* `Quad` and
  `PolychromSprite` for exactly this. Rule: adding a scene field means
  editing `scene.rs`, the producer, and `shaders.hlsl` in one change, and
  debug runs won't catch a release-only divergence (next bullet).
- **Debug/release shader paths differ**: debug compiles `shaders.hlsl` at
  runtime (`D3DCompileFromFile`); release embeds build-time fxc output, and
  the build **hard-fails** without `fxc.exe` (`gpui_windows/build.rs`:
  `GPUI_FXC_PATH` → `where.exe` → newest Windows SDK). A machine that builds
  debug fine can fail release builds; a shader bug can reproduce in only
  one configuration.

### 7.2 Text and fonts

DirectWrite resolves a family by name and **cannot rescue an unresolvable
primary family through fallback** (`direct_write.rs`) — commit 919a632 moved
the caption glyphs to `Segoe MDL2 Assets` because `Segoe Fluent Icons` does
not exist on Windows 10 and rendered tofu (`ui/shell/titlebar.rs`). Custom
fallback mappings merge *after* the system fallback (the CJK/glyph-miss
path). Embedded fonts (Geist et al.) are immune; any new OS-font dependency
must name a face present on Windows 10 or embed one.

### 7.3 Non-client area, DPI, and input

- The overlay titlebar needs `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition
  (opaque swapchains can't do per-pixel alpha at the top). `WM_NCCALCSIZE`
  keeps the frame's top when the titlebar is hidden and re-adds thickness
  when maximized — "content slides under the caption when maximized" is
  this handler. `WM_NCHITTEST` computes resize borders from
  `GetSystemMetricsForDpi` and maps Holt's `WindowControlArea` to native
  `HT*` results, which is what keeps Snap Layouts working on the custom
  buttons (46×32 metrics, `titlebar.rs`).
- Holt's Windows titlebar is a normal-flow 38px strip (commit a728e85), not
  an overlay; every top inset must come from `Theme::chrome_top_inset()`
  (`ui/theme.rs`) — the migration list (shell, panes, chat_list, right_pane,
  transcript render/viewport, tabs) is the checklist for any new
  top-anchored surface.
- DPI: PerMonitorV2 via manifest; `WM_DPICHANGED` rescales and refits
  (maximized windows take the manual work-area path). Fractional scale
  factors on Windows round to integers (`MDT_EFFECTIVE_DPI` / 96) — pixel
  snapping bugs that "only happen at 125%/150%" start here.
- Input quirks with in-code receipts: `ctrl-shift-0` cannot be delivered by
  Windows at all (`events.rs`); stale modifier state after Alt-Tab is
  manually reset on activation; IME composition arrives as UTF-16 and is
  converted (surrogate pairs handled); forcing foreground synthesizes an Alt
  keypress — a quirk that hides under a debugger.
- Platform no-ops to remember: `register_url_scheme` (so `holt://` links
  don't open Holt on Windows), `hide_other_apps`/`unhide_other_apps`
  (`unimplemented!`), native anchored popups (rejected → in-window
  fallback), mixed file+folder selection in one dialog,
  underline/strikethrough/inline-object draws in DirectWrite.
- `reveal_path` uses `SHOpenFolderAndSelectItems` with a ShellExecute
  fallback for its spurious `ERROR_FILE_NOT_FOUND` — if "reveal in Explorer"
  ever breaks, both halves are already there.
- `ui/file_dialog.rs`'s pickers are macOS NSOpenPanel only — the provider
  **logo picker silently does nothing on Windows** (gpui's IFileOpenDialog
  path exists but is not what this code calls).

## 8. Network, TLS, self-update

- TLS is rustls everywhere (no OpenSSL in the graph) — the port-clean
  corner. `crates/core` uses `rustls-tls-native-roots`, i.e. the **Windows
  certificate store**: corporate intercept roots that are installed for the
  user just work; ones that aren't fail with chain errors macOS CI never
  saw.
- Self-update is macOS-only end to end: `update.rs` `bundle_path` requires
  an `.app` bundle layout (returns `None` on Windows, so the check loop
  never even starts), the swap shells out to `hdiutil`/`codesign`/`ditto`,
  and asset names are `.dmg`s. The site already ships a per-platform
  download page (commit d351cb7) — a Windows update path (replace-on-restart
  pattern) is an open gap, not a regression.

## 9. Known-open Windows gaps (do not re-diagnose these)

Survey update 2026-10-23: the cross-platform branch has since closed most
of the original gaps — terminals flags, MCP `.cmd` resolution, Job Objects,
native noreplace rename, drive enumeration, core's home chain, the
settings pickers, and the console-window pair. What remains open:

| Gap | Where | Status |
| --- | --- | --- |
| Trash unavailable | `trash.rs` | open (macOS-only impl; needs IFileOperation) |
| Self-update unavailable | `update.rs` | open (macOS-only impl; needs a Windows replace-on-restart path) |
| Terminal `has_running_jobs` always false | `terminals.rs` unix-only session scan | open (documented on terminals page) |
| `holt://` scheme not registered | gpui_windows `register_url_scheme` no-op | open |
| Credential files not hardened (no ACLs) | `credentials.rs`, `mcp/config.rs`, `provider_*.rs` | unix-only 0600 |
| MCP children get no login-PATH injection | `shell_env::login_path` None on Windows | by design (registry PATH); watch for minimal-PATH launchers |
| OS dialogs cannot mix files and folders | `ui/file_dialog.rs` `prompt_for_files_and_dirs` | worked around (two sequential passes) |
| cmd.exe output decodes in the OEM code page | `Shell::exec` lossy UTF-8 decode | open (set `CHCP 65001` or decode the page for non-ASCII fidelity) |
| `core.autocrlf` CRLF vs byte-exact checkpoints | `git/checkpoints.rs` | open (no `.gitattributes`; tests that byte-compare working-tree fixtures are platform-sensitive) |

## 10. Testing on Windows

- CI (macOS) gates `cargo test --workspace`; nothing Windows runs there.
  The Windows check is a local `cargo test --workspace` — and note the
  engine integration suite is compiled behind a unix gate
  (`tests/all/main.rs`), so a green Windows run exercises fewer tests than
  the same run on macOS. Treat "the Windows suite passed" as a weaker
  statement and prefer adding `cfg`-balanced unit tests near the code.
- Patterns already in tree for dual-platform tests: `let root = if
  cfg!(windows) { "C:/repo" } else { "/repo" }` (`gate.rs`, `decode.rs`
  tests); the `addressed()` separator-fold helper for fixture paths
  (`skills.rs` tests); never asserting on raw `canonicalize` output;
  splitting command output on `\r\n` where cmd.exe is involved.
- `HOLT_TEST_LIVE=1` gates live-provider suites exactly as on macOS.
- Build environment needs: MSVC toolchain (libgit2 via cc), and for release
  builds `fxc.exe` from a Windows SDK (§7.1). A fresh-box build failure is
  usually one of those two, not Holt code.

## 11. Triage table — symptom → first suspects

| Windows-only symptom | Class | Entry points |
| --- | --- | --- |
| Write/read refused as "outside workspace" on a valid path | 1 | `tools.rs` `to_absolute`/`resolve_landing`, `files.rs` `resolve_inside_root` |
| "Traverses a symlink" refusal on a plain file | 1/2 | `tools.rs` `write_to_file` landing check (case mismatch fails safe) |
| Skill chip/link doesn't resolve or render | 1 | `skills.rs` `by_path`, `decode.rs` `skill_read_part` |
| Grant re-asks after always-allow | 2 | `gate.rs` `GateGrants` resolved-spelling match |
| Save/rename fails "access denied" intermittently | 5 | AV/indexer sharing violation on the atomic-rename sites (§3.1) |
| Directory fsync `ERROR_ACCESS_DENIED` | 5 | must use `store.rs` `sync_dir` (§3.2) |
| MCP server "did not initialize within startup timeout" | 3 | resolved bare names now walk PATH+PATHEXT (`shell_env::resolve_command`); remaining failures: minimal launcher PATH, startup timeout |
| Terminal session dies instantly | 3 | `-l -i` reaches only POSIX shells (`shell_env::interactive_login_args`) |
| Orphaned processes after interrupt/quit | 4 | Job Object kill-on-close (`tools.rs` `ShellJob`) |
| Non-ASCII shell output is mojibake | 3 | OEM code page vs UTF-8 lossy decode (§4.4, still open) |
| Every file shows as fully changed in diff/checkpoint | 3/6 | `core.autocrlf` CRLF (§4.4, §6) |
| Sprites/paths vanish partway down the frame | 6 | structured-buffer SRV chunking (`directx_renderer.rs`) |
| Geometry/UV garbage after a scene-struct change | 6 | HLSL↔scene alignment (`shaders.hlsl` vs `scene.rs`) |
| Tofu boxes in caption buttons (Win10) | 6 | `Segoe MDL2 Assets` resolution (`titlebar.rs`) |
| Content slides under the titlebar when maximized / DPI change | 6 | `WM_NCCALCSIZE`, `WM_DPICHANGED`, `chrome_top_inset` avoid points |
| A keybinding with `ctrl-shift-0` never fires | 6 | Windows cannot deliver it (`events.rs`) |
| File sidebar / git panel stops updating | 5 | watcher arm failure degrades silently (§3.5) |
| Duplicate checkout/change-set history for one repo | 1/6 | `checkout_identity` spelling hash (§6) |
| Logo picker button does nothing | — | fixed: pickers use gpui's dialog off macOS (`ui/file_dialog.rs`) |
| Delete-to-trash errors | — | `trash.rs` macOS-only |
| Update check never runs | — | `update.rs` `bundle_path` `None` |
| Console window opens beside the app | 4 | fixed: release-only `windows_subsystem` + `CREATE_NO_WINDOW` on every spawn (§5) |
| Fresh box: `cargo build` fails before Holt code | — | missing MSVC toolchain or `fxc.exe` (§10) |

## 12. Sources

- Working tree, branch `feat/issue-52-cross-platform`, 2026-10-23 — all
  `file` references above; the in-flight diff supplied the
  addressed-vocabulary rework and its comments.
- Commits that fixed each class, as precedent to copy:
  `94dce85` (first linux+windows run), `083d4d3` (USERPROFILE home),
  `398e17f` (durable save + Windows test suite, issue #52),
  `f4e6689` (D3D chunking + HLSL alignment), `919a632` (caption metrics +
  MDL2), `a728e85` (menu-bar strip + `chrome_top_inset`).
- [ARCHITECTURE.md](../../ARCHITECTURE.md) for the boundaries this page's
  rules sit inside; [docs/research/gpui.md](gpui.md) for the framework
  model the rendering section assumes.
