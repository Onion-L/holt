---
name: issue
description: "Open, read, search, or update GitHub issues via `gh`. For bug reports, feature requests, roadmap items."
---

# GitHub Issue

GitHub issues (repo resolved by `gh` from the git remote) are the project's
only issue tracker — bug reports, feature requests, roadmap items.

## Safety

Creating, editing, commenting on, or closing a GitHub issue is an external
publishing action: show the full title/body draft and get explicit user
confirmation before running the command. Read-only commands need none.

If `gh auth status` fails, stop and tell the user to run `gh auth login`;
never work around it.

## Read

- List: `gh issue list --state all --limit 30`
- View with comments: `gh issue view <N> --comments`
- Search: `gh search issues --repo Onion-L/holt "<terms>"`

## Create

1. Collect the irreproducible information first:
   - bug → the symptom (one line) and the **exact trigger conditions**;
   - feature → the idea and the **context that sparked it**.
2. Pick the type label (mandatory, exactly one — see below).
3. Draft the title and body (templates below) and show them verbatim,
   including the chosen label.
4. After confirmation, write the body to a temp file and run
   `gh issue create --title "<title>" --body-file <tmpfile> --label <type>`. Using a body
   file avoids shell-quoting bugs; quote the title yourself.

Title: one imperative line, e.g. `crash when opening a worktree with no
branches`. No `Bug:` prefix — labels carry classification.

Body: use the template for the chosen type. Only the two core sections of
each template are required; everything else is optional. Capture beats
formality — if details are thin, file what exists and say so in the body;
do not block filing on a perfect write-up.

### Bug body (Spec format)

```markdown
## Symptom
<one line: what is wrong>

## Trigger
<the specific conditions that make it appear — exact steps, state, input.
For a bug that only shows under rare conditions, this section IS the issue;
write it precisely.>

## Acceptance Criteria
- <concrete verifiable behavior: given state X, when triggering Y, expect Z>
- <edge cases: e.g. empty inputs, network disconnects, invalid payloads>

## Fence
- <allowed scope: which crates, modules, or files may be touched>
- <forbidden scope: boundaries or interfaces that must NOT be altered>

## Notes
<optional: expected vs actual if non-obvious, build/version if it matters,
related issues>
```

### Feature body (Spec format)

```markdown
## Idea
<what you want>

## Context
<what you were doing when the idea came up — the scene that motivated it.
This is what makes the idea actionable later.>

## Acceptance Criteria
- <verifiable behaviors and observable outputs needed to consider this done>
- <verification command or test to prove it>

## Fence
- <allowed scope: owning subsystem and target files>
- <forbidden scope: crates, public APIs, or database schemas that must stay intact>

## Notes
<optional: rough sketch, related issues>
```

For `question` / `documentation` / other types there is no fixed body; keep
the `## Summary` + `## Notes` shape unless the user asks for more structure.

Labels: check existing labels with `gh label list`. Ask before creating new labels.

### Type labels (mandatory, exactly one)

Every issue gets exactly **one** type label, picked at creation time:

| Label | Use when |
|-------|----------|
| `bug` | Something is broken or behaves incorrectly |
| `enhancement` | New feature or improvement to existing behavior (features merge here) |
| `documentation` | Docs, changelog, site content |
| `accessibility` | Barrier affecting people with disabilities |
| `security` | Security finding |
| `question` | It is really a question, not a defect or request |

Rules:
- One type per issue — never combine `bug` with `enhancement`.
- These map to the commit vocabulary: `bug`≈fix, `enhancement`≈feat, `documentation`≈docs.

### Execution status labels (optional, lifecycle gating)

| Label | Color | Use when |
|-------|-------|----------|
| `spec:ready` | `#0e8a16` | Spec is complete with Acceptance Criteria and Fence; ready for implementation |
| `blocked:agent` | `#e11d48` | Agent cannot proceed — missing external dependency, contract conflict, or capability wall |

If these labels do not exist yet on the remote repo, confirm with the user before running `gh label create <name> --color <hex> --description <desc>`.

## Block an issue (Surrender flow)

When an agent executing an issue cannot proceed, it must **never** fake success, delete tests, or patch unrelated code. It must surrender cleanly:

1. Stop making speculative code changes; revert dirty worktree state if needed.
2. Label the issue as blocked:
   `gh issue edit <N> --add-label "blocked:agent"`
3. Leave a structured evidence comment on the issue:

```markdown
### 🛑 Implementation Blocked

- **Category**: `[capability | contradiction | tooling/environment]`
- **Evidence**:
  - Command run: `<exact command>`
  - Error output / observation: `<concise logs or conflicting file paths and line numbers>`
- **Blocker**: Why this stops the acceptance criteria from being met safely.
- **Decision needed**: Specific choice or clarification requested from human maintainer.
```

4. Confirm and post the comment:
   `gh issue comment <N> --body-file <tmpfile>`

## Update / close

State changes (close, reopen, label, edit, comment) are mutations: state the
exact command, confirm, run. Never close an issue the user did not ask to
close.

When the work on an issue finishes, offer to comment a closing summary on it
(confirm the comment text first).
