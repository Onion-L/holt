# Subsystem references

These pages describe how a part of Holt **runs today**: its current
responsibilities, runtime paths, state, contracts, and failure behavior.
They are living documents — updated in the same change as the code they
describe, never after the fact, and never in advance of it.

## When to read

Three triggers, in order of frequency:

1. **Before changing code.** Start with the page relevant to your task,
   then follow its source links to verify the implementation you are about
   to change — the page gives the contract, the links give the truth, and
   both must agree before the page is read. Reading the page and reading
   the code are bound; neither alone is enough.
2. **Before preparing a PR.** The description names each affected
   subsystem's observable change, trigger, and before/after (see
   Describing a change below) — you cannot write that without knowing what
   the page says.
3. **When reviewing a change.** The checklist starts by identifying the
   affected subsystem and the actual runtime path, and checks that changed
   contracts appear in the owning page with links that still resolve.

Two implied moments: **diagnosing a report** (the page is how you tell
which code actually runs — paired with AGENTS.md's "What the engine
actually serves"), and the reverse of reading — **a change that alters a
documented contract writes back to the owning page in the same PR**.

The negative space is equally explicit: an internal refactor that
preserves the documented contracts requires no page work — but judging
"does this change a contract?" already requires knowing the contract, so
a scan first stays worth its cost.

**When the page and the code disagree**, the code is what runs: fix
whichever is wrong, in the same PR. If you cannot tell which one is
wrong, stop and ask — never pick a side silently.

| Group | Pages |
| --- | --- |
| **agent-runtime** — neutral machinery: any agent system could use it unchanged. Contracts here hold regardless of Holt's product shape. | [run-loop](run-loop.md) · [persistence](persistence.md) · [toolset-mechanics](toolset-mechanics.md) · [provider-transport](provider-transport.md) · [subagents](subagents.md) |
| **agent-application** — Holt's product policy: prompts, modes, skills, goals. Change these and you change what the product is; users can configure them. | [prompts-skills](prompts-skills.md) · [permission-gate](permission-gate.md) · [modes](modes.md) · [goals-routines](goals-routines.md) · [conversation-policy](conversation-policy.md) |
| **app** — surfaces with no agent involvement: they exist as-is even if every model disappeared tomorrow. | [terminals](terminals.md) · [workspace-git](workspace-git.md) · [workspace-files](workspace-files.md) · [shell-registry](shell-registry.md) |

The test for the split: anything another system could reuse without change
is runtime; anything a user customizes (prompts, modes, schedules) is
application; anything the agent never touches is app.

A subsystem spans whatever code its behavior spans — crates, modules,
files. A page follows the behavior, never the directory layout, and never
lists every function: only the parts that carry contract meaning.

## Page anatomy

Every page has five parts:

1. **Title + one-line definition + fence** — what the subsystem is, then an
   explicit "not here" paragraph naming the neighbors that own the
   bordering behavior.
2. **Responsibilities and sources of truth** — a table of Part /
   Responsibility / Source, linking files and named symbols.
3. **Runtime path** — numbered steps following one real request or
   lifecycle end to end.
4. **Constraints, state, and failure behavior** — the state the subsystem
   owns and its transitions, the invariants, and the failure semantics the
   signatures alone cannot express.
5. **Verification** — commands that actually run, and what each proves.

A page stays small enough to read before starting a change. If a section
outgrows a screen, split the subsystem or link out; do not grow the page.

## Maintaining a page

- Update the owning page **in the same PR** when a change alters a
  documented responsibility, interface, state transition, runtime path,
  compatibility rule, or failure behavior.
- An internal refactor that preserves those contracts needs no prose
  change.
- Pages document current behavior only. Proposals belong in their issue or
  PR.
- Create a new page when a behavior domain earns one: a maintainer has to
  cross crate boundaries (or hunt through a file like `rpc/run.rs`) to
  follow it, and it has a runtime path or state of its own. A single
  module's behavior that its source file already explains does not need a
  page — extend the neighboring page or leave it to comments.
- **One fact, one home.** Link to it from everywhere else; never copy it.
  If a fact appears in two places, one of them is wrong — fix the wrong
  one; prose that duplicates what code or another page already says gets
  deleted, not paraphrased.

## Deleting a page

- **A page dies with its subsystem**, in the same PR as the removal —
  pages for removed behavior are deleted, not archived.
- A research note with no landing (no decision entry cites it, nothing
  references it) is deleted rather than kept.
- When deleting or renaming any document, fix every reference to it in
  the same commit; grep for the old name.

| Layer | Owns | Frequency |
| --- | --- | --- |
| `ARCHITECTURE.md` | topology and hard crate fences | rarely |
| these pages | how it runs now | with the code |
| `docs/DECISION.md` | why it was decided, and the alternatives rejected | once — a changed decision appends a new entry |
| code comments | the local invariant | with the code |

- Link to files and named symbols, not implementation details that churn.
- A documentation update does not prove the implementation. Behavior
  changes still ship tests, and a page's Verification commands must run.

## Describing a change

For each subsystem a PR touches, the description states the trigger, the
observable before/after behavior, and the verification evidence — linking
the owning page and the implementation. A refactor that preserves
documented behavior says so explicitly. This is an author-provided account
the reviewer checks against the diff, not an automatically proven
compatibility verdict.

## Reviewing a change

1. Identify the owning page and the actual runtime path, including any
   cross-group boundary the change crosses.
2. Compare the claimed before/after behavior with the code, tracing
   callers and state transitions beyond the changed lines.
3. Check that changed contracts appear in the owning page and that its
   source links still resolve.
4. Check that the cited tests were run and actually exercise the claim;
   say so when they were not.
