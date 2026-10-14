---
name: pr
description: "Open and manage pull requests on the project's GitHub repo via the `gh` CLI: branch push, PR creation, checks, and merge. Use when the user asks to open/create/submit a PR, check PR or CI status, or merge a PR. For code and spec review, see the `code-review` skill. Releases are NOT done via PR — they follow the tag workflow in AGENTS.md."
---

# GitHub PR

Delivery-flow skill: push a branch, open a PR linked to an Issue Spec, report
checks, and merge. Code and contract audits are performed by `code-review`.
The default workflow in this repo is still direct commits to `main` plus release
tags (AGENTS.md, "Releases"); open a PR only when the user asks for review.

## Safety

Pushing a branch, creating a PR, and merging are external actions: show the
exact `git push` / `gh pr create` / `gh pr merge` command and the full PR
title/body draft, and get explicit confirmation before running either. Never
force-push. Never merge unless the user explicitly says to.

If `gh auth status` fails, stop and tell the user to run `gh auth login`.

CI (`.github/workflows/ci.yml`) runs on every branch push and PR, gating on
`cargo test --workspace` only (macOS; no separate check/clippy job).

## Read (no confirmation)

- `gh pr list`
- `gh pr view <N|branch>` / `gh pr view <N|branch> --comments`
- `gh pr checks <N|branch>`
- `gh pr diff <N|branch>`

## Open a PR

Pre-flight:

1. `git status` — uncommitted changes must be committed or explicitly
   excluded; never push unreviewed user changes.
2. The work is on a branch, not `main` (the default branch). If it sits on
   `main`, ask before creating a branch from it.
3. Identify the origin Issue number (e.g. `#12`). Every PR must link to its
   guiding Issue Spec.
4. Commit messages follow `type(crate): summary`; scopes in use: `core`,
   `engine`, `ui` — comma-joined when a change spans several (AGENTS.md).
5. If the change touches any documented contract, its owning subsystem page
   (`docs/subsystems/*.md`) must be modified in the same branch.
6. `cargo check --workspace` and the focused tests pass, or record what was
   not run.

Steps:

1. Confirm, then `git push -u origin <branch>`.
2. Draft the title and body (templates below) and show them verbatim.
3. After confirmation, write the body to a temp file and run
   `gh pr create --title "<title>" --body-file <tmpfile>`. Add `--draft` only
   if the user wants review-before-ready.
4. Report the returned PR URL. Do not merge.
5. Hand off to the `code-review` skill for spec compliance and architecture check.

Title: same convention as commits — `type(crate): summary` describing the
whole change, e.g. `fix(ui): keep wheel scroll inside nested lists`.

Body template:

```markdown
## Issue
Closes #<N> (or Relates to #<N>)

## What / Why
<one short paragraph: the user-visible change, trigger, and motivation>

## Subsystem
<owning subsystem page: e.g. docs/subsystems/conversation-policy.md, or "None: internal refactor preserving documented contracts">

## Changes
- <one line per commit, from `git log origin/main..HEAD --oneline`>

## Verification
- <commands proving each Acceptance Criterion from the linked Issue Spec>
- <checks skipped, and why>

## Changelog
- <"none (internal)" or a one-line candidate for site/changelog.html at release time>
```

## Checks / merge

- Report `gh pr checks` results; GitHub CI may differ from local runs.
- Review: invoke the `code-review` skill to audit the diff against the linked
  Issue's Acceptance Criteria and Fence before merging.
- If asked to merge: confirm the exact command first —
  `gh pr merge <N> --squash --delete-branch` unless the user specifies
  otherwise — and note that a release bump/changelog commit still happens
  separately on `main` per the release workflow.
