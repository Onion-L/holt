# Subsystem references

These pages describe how a part of Holt **runs today**: its current
responsibilities, runtime paths, state, contracts, and failure behavior.
They are living documents — updated in the same change as the code they
describe, never after the fact, and never in advance of it.

Before changing a documented subsystem, read its page and verify the linked
sources against the code you are about to edit.

| Group | Pages |
| --- | --- |
| **agent-runtime** — neutral machinery: any agent system could use it unchanged. Contracts here hold regardless of Holt's product shape. | run-loop · persistence · toolset-mechanics · provider-transport · subagents *(planned)* |
| **agent-application** — Holt's product policy: prompts, modes, skills, goals. Change these and you change what the product is; users can configure them. | prompts-skills · permission-gate · modes · goals-routines · conversation-policy *(planned)* |
| **app** — surfaces with no agent involvement: they exist as-is even if every model disappeared tomorrow. | [terminals](terminals.md) · workspace-git · shell-registry *(planned)* |

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

| Layer | Owns | Frequency |
| --- | --- | --- |
| `ARCHITECTURE.md` | topology and hard crate fences | rarely |
| these pages | how it runs now | with the code |
| `docs/adr/` | why it was decided, and the alternatives rejected | once, then immutable |
| code comments | the local invariant, plus an ADR number when one exists | with the code |

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

## Relationship to ADRs

Each page links the ADRs behind its behavior (a "decision record" row or a
Decisions section). New decisions supersede old ones by writing a **new**
ADR and a banner on the old one — never by amending the old ADR in place.
An ADR deleted as truly dead leaves one tombstone line in its owning page
("model-setup-applies-stored-proposals: removed, superseded by
provider-mode-is-a-chat-mode"), so a decision never vanishes silently.

## Reviewing a change

1. Identify the owning page and the actual runtime path, including any
   cross-group boundary the change crosses.
2. Compare the claimed before/after behavior with the code, tracing
   callers and state transitions beyond the changed lines.
3. Check that changed contracts appear in the owning page and that its
   source links still resolve.
4. Check that the cited tests were run and actually exercise the claim;
   say so when they were not.
