# Project-less chats belong to the Home space, not to a nullable space

A chat that is not bound to any user project is a real need: ask a question,
configure providers, run web research — none of it wants a folder picker in
the way. Holt had already tried two answers and retreated from both. The
first (pre-import) minted project-less sessions with cwd `~`; the code that
blocked them (`send_blocked`: "project-less `~`-cwd sessions are no longer
mintable from the canvas") outlived the reasoning, and half the machinery it
disabled (`state.no_project`, its persisted `composer-defaults` flag, the
sidebar's first-class `space_id: None` rows) stayed behind as dead state.
Neither approach is adopted. ZCode's model is: **there is no such thing as a
workspace-less conversation** — an engine-owned default workspace absorbs
them.

## Decisions

- **"No project" is the Home space.** A single Space row, id `home`
  (the ADR-0038 deterministic-id convention applied to a singleton), path
  `<data_dir>/workspace/default`, name "Home", created (folder + row) and
  repaired idempotently at every engine boot. No `purpose` field: the id is
  the marker, exactly like `wt-<chatId>` rows.
- **Every chat carries a space id.** `createChat` keeps its optional
  `spaceId` param, but boot adopts every legacy `space_id: None` row into
  Home; a `~`/unset cwd is rewritten to the Home folder, an explicitly
  chosen cwd survives. After adoption the `None` state is unreachable from
  the UI, and the UI's spaceless rendering branches (overview rows, `~`
  folder labels, the `cwd:` FileStateMap key, `no_project` selection state)
  are deleted rather than maintained.
- **The Home space is engine-owned.** `createSpace` refuses the id,
  `renameSpace`/`deleteSpace` refuse the row; the sidebar's context menu
  offers nothing on it. The project picker never lists it — its seat is the
  "Work outside a project" action row, which selects it like any other
  space pick and persists through the same remembered-defaults path.
- **No special cases downstream — one carve-out.** A Home chat stamps the
  Home folder as its working directory like any space chat; the Git panel,
  branch picker, Changes, and diff surfaces stay gated on git detection (a
  `git init` in the Home folder lights them up); skill roots and file
  browsing follow the cwd as usual. The carve-out: `/init` is not offered
  and not executable in the Home space — bootstrapping an AGENTS.md is a
  project act, and the UI (popup row + interception) drops it there. First boot shows the canvas immediately — the old blocking
  "Add a project" onboarding is demoted to a dismissible hint shown only
  while Home is the only space.
- **No post-hoc re-binding.** A chat's space is fixed at creation (the
  glossary's working-directory rule stands); moving chats between spaces is
  out of scope. Session-worktree chats still re-parent at admission
  (ADR-0038) — that path is unaffected.

## Consequences

- `~` is never a chat cwd again: content search roots, the project skill
  root (which collided with the personal root at `~/.agents/skills`), and
  file browsing now anchor on a scratch folder Holt owns instead of the
  user's real home.
- A fresh data dir boots with exactly one space; tests that assumed an
  empty `WatchSpaces` frame assert the Home row instead.
- Legacy spaceless rows migrate once, on boot; their next Turn runs in the
  Home folder. Files they left in the real home directory stay there.
- Holt is a single-device product today; the glossary's "synced" phrasing
  on Space is forward-looking bookkeeping, not a live subsystem (noted
  here, unchanged in the glossary).
