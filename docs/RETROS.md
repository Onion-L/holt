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

- Before deleting prose as "duplicated," verify every fact in it has
  another home — a section is only removable when each of its facts lands
  somewhere first. A pointer-style rewrite of AGENTS.md claimed zero
  duplication and dropped three code-style bullets that existed nowhere
  else; they had to be rebuilt from git history
  (c526a91). Seen 1× (2026-10-14).

## Moved

(none yet)
