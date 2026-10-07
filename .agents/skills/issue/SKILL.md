---
name: issue
description: "File, read, and manage issues on the project's GitHub repo via the `gh` CLI. Use when the user asks to open/report/file an issue on GitHub, check or search GitHub issues, or promote a local .scratch spec to a GitHub issue. Development-process tickets stay local in .scratch/ — do not use this skill for those."
---

# GitHub Issue

Two trackers, one job each:

- **GitHub issues** (repo resolved by `gh` from the git remote): durable,
  externally visible record — bug reports, feature requests, roadmap items.
- **`.scratch/`** (see `docs/agents/issue-tracker.md`): development process —
  specs, implementation tickets, wayfinding. Never publish these to GitHub
  unless the user explicitly asks.

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

### Bug body

```markdown
## Symptom
<one line: what is wrong>

## Trigger
<the specific conditions that make it appear — exact steps, state, input.
For a bug that only shows under rare conditions, this section IS the issue;
write it precisely.>

## Notes
<optional: expected vs actual if non-obvious, build/version if it matters,
related issues, local .scratch paths>
```

### Feature body

```markdown
## Idea
<what you want>

## Context
<what you were doing when the idea came up — the scene that motivated it.
This is what makes the idea actionable later.>

## Notes
<optional: rough sketch, related issues, local .scratch paths>
```

For `question` / `documentation` / other types there is no fixed body; keep
the `## Summary` + `## Notes` shape unless the user asks for more structure.

Labels: only use labels that already exist (`gh label list`); ask before
creating new ones.

### Type labels (mandatory)

Every issue gets exactly **one** type label, picked at creation time:

| Label | Use when |
|-------|----------|
| `bug` | Something is broken or behaves incorrectly |
| `enhancement` | New feature or improvement to existing behavior |
| `documentation` | Docs, changelog, site content |
| `accessibility` | Barrier affecting people with disabilities |
| `security` | Security finding |
| `question` | It is really a question, not a defect or request |

Rules:

- One type per issue — never combine `bug` with `enhancement`; if both apply,
  pick the dominant one and mention the other in the body.
- These map to the commit vocabulary: `bug`≈fix, `enhancement`≈feat,
  `documentation`≈docs.
- Do **not** confuse type labels with triage state. Lifecycle roles
  (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`,
  `wontfix`) live on the local `.scratch/` `Status:` line
  (`docs/agents/triage-labels.md`), not as GitHub labels.
- Non-type labels (`good first issue`, `help wanted`) are optional extras.

## Linking with the local tracker

When GitHub issue #N is about to be worked on:

- create `.scratch/<feature-slug>/` per the local tracker doc, and
- put `GitHub: #N <url>` near the top of the spec or first ticket.

When the work finishes, offer to comment a closing summary on the issue
(confirm the comment text first).

## Update / close

State changes (close, reopen, label, edit, comment) are mutations: state the
exact command, confirm, run. Never close an issue the user did not ask to
close.
