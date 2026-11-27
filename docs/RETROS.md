# Retros — the lessons log

A pure error-pattern archive: what merged work got wrong, and the rule that
would have caught it. Successful work, correct decisions, and selections
that caused no rework **never** land here — that is
[DECISION.md](DECISION.md)'s opposite face (it records why right; this
records what counts as wrong).

<!-- reviewed-through: (none yet — the first review starts here) -->

## How entries are made

The day's merged commits are reviewed along history — nightly by a routine
when one exists, or before each session's first change otherwise — looking
for exactly these shapes:

1. **Fixed twice** — a fix that needed another fix. The second fix is the
   evidence the first was incomplete: it missed a package, a surface, or a
   step of the flow.
2. **Should not have merged** — shipped past a red it explained away, or
   with a known issue left unfixed.
3. **Verified too thinly** — fixed without reproducing on the surface where
   it was reported (a GUI report verified in the engine, a WSL report
   verified on macOS).
4. **Process incidents** — wrong files shipped under a subject, a known
   hole left in a document until the retro found it.

An author who catches their own miss adds the entry directly; the review
catches the rest.

## Entry format

One bullet: the rule, the why, the evidence (commit or issue), ending with
the seen-counter:

```
- <the rule>. <why it exists>. <evidence>. Seen N× (dates)
```

`Seen` counts **different days the review found it**, not occurrences.

## Promotion

A lesson seen on 3 or more separate days has proven it recurs: it moves
into [CODE-STANDARDS.md](CODE-STANDARDS.md) as a standing rule, and this
file keeps only a one-line "Moved" note. Nothing becomes a standard by
intuition — three strikes is the only gate.

## Entries

- Never read an entity from a child render while that same entity is already
  being updated; pass the derived state down from the caller. The usage ring's
  `pickers.read(cx)` inside `Pickers::render_footer` panicked on startup, while
  its isolated preview used a different owner and stayed green; the regression
  test must mount a full Shell with an existing usage frame (user report).
  Seen 1× (2026-11-18).

- Before deleting prose as "duplicated," verify every fact in it has
  another home — a section is only removable when each of its facts lands
  somewhere first. A pointer-style rewrite of AGENTS.md claimed zero
  duplication and dropped three code-style bullets that existed nowhere
  else; they had to be rebuilt from git history
  (c526a91). Seen 1× (2026-10-14).
- A reimplementation of an algorithm is not a test of it. When the real
  harness is blocked, the blocker is the finding — the URL-wrap fix's
  replica script "passed" while the vendored suite, once runnable, caught
  the same dangling slash surviving through the hard-break fallback path
  the replica never modeled (issue #44, fixed before commit).
  Seen 1× (2026-10-15).
- A reference sweep for a deleted or renamed document greps every form
  the name takes — the path, the path split across a comment line break,
  the bare page name in prose ("see toolset") — not just one. A sweep
  that claimed "every dead file reference" missed
  `docs/research/mugen-pretext.md` wrapped across two lines in
  `transcript/model.rs`, and five subsystem pages still named the
  pre-split `conversation-runtime`, `toolset`, and `ui-shell` pages
  (cffdd0c, fixed again in 9219a26). Seen 1× (2026-10-14).

- An "unused variable" warning for state created on purpose is a feature
  missing its wiring — investigate before deleting. The background-process
  titlebar button computed `menu_open` for its active highlight and never
  wired it; the compiler flagged the variable, the variable was deleted to
  silence the warning, and the button shipped never highlighting while its
  popover was open — found by the user's eye, not by any check (user report).
  Seen 1× (2026-11-04).
- A flaky test is diagnosed by tracing the frames it actually waits for, never
  by widening its timeout. The engine harness's 10 s → 30 s bump on a
  cross-platform CI hang (ec5f19c) read the symptom as a slow runner; the test
  waited for an `idle` frame that could never come, because the restarted
  engine resumed the interrupted Turn, the resume consumed the scripted reply,
  and its own Turn then errored — a state that had already settled before the
  test subscribed. A test that waits for a state must read the watch's current
  value (its opening frame), not only its next change
  (`crates/engine/tests/all/common/mod.rs`, `wait_for_chat_status`).
  Seen 1× (2026-10-23).
- A refresh of shipped data is verified on an install that booted before,
  never only on an empty data dir. Boot wrote its own copy of the catalog
  into `provider-store.json`, and that copy stood in front of every later
  catalog — yet every fixture starts empty, so the copy was the catalog and
  no test could see the shadow: the 2026-11 models.dev refresh passed three
  reviews while the OpenAI picker still listed GPT-4 beside GPT-6 (user
  report; ADR-provider-overlay-carries-overrides-not-a-snapshot).
  Seen 1× (2026-11-27).
- A filter over state that is still loading is verified with it on its way,
  never only once it has landed. The fetch dialog "drops the ids the
  provider already offers" by reading the panel's model list as it opens:
  clicked before the list landed, it offered the provider's own models
  back, and Add filed a blank record over the catalog's — a `glm-5.4` with
  max output 8,192 over Z.AI's 131,072 — while every test opened the
  dialog from a settled list (user screenshot;
  ADR-fetched-models-adopt-the-catalogs-record).
  Seen 1× (2026-11-27).
- A content-sized layout must be verified on the rendered surface before it
  gets a fixed cache frame; element bounds and interaction tests alone do not
  catch overflowing glyphs. The fetch dialog's 56px cached rows painted over
  one another while the full UI suite passed; their natural height was 60px,
  so the fixed frame and the cache were removed (user screenshot).
  Seen 1× (2026-11-27).

## Moved

(none yet)
