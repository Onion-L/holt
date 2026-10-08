---
name: pr
description: "Open and manage pull requests on the project's GitHub repo via the `gh` CLI: branch push, PR creation, checks, and merge. Use when the user asks to open/create/submit a PR, check PR or CI status, or review a PR. Releases are NOT done via PR — they follow the tag workflow in AGENTS.md."
---

# GitHub PR

Review-flow skill: push a branch, open a PR, report checks. The default
workflow in this repo is still direct commits to `main` plus release tags
(AGENTS.md, "Releases"); open a PR only when the user asks for review.

## Safety

Pushing a branch and creating a PR are external actions: show the exact
`git push` / `gh pr create` command and the full PR title/body draft, and get
explicit confirmation before running either. Never force-push. Never merge
unless the user explicitly says to.

If `gh auth status` fails, stop and tell the user to run `gh auth login`.

CI (`.github/workflows/ci.yml`) runs on every branch push and PR, gating on
`cargo check` + `cargo test --workspace`.

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
3. Commit messages follow `type(crate): summary`; scopes in use: `engine`,
   `ui`, `engine,ui` (AGENTS.md).
4. `cargo check --workspace` and the focused tests pass, or record what was
   not run.

Steps:

1. Confirm, then `git push -u origin <branch>`.
2. Draft the title and body (templates below) and show them verbatim.
3. After confirmation, write the body to a temp file and run
   `gh pr create --title "<title>" --body-file <tmpfile>`. Add `--draft` only
   if the user wants review-before-ready.
4. Report the returned PR URL. Do not merge.

Title: same convention as commits — `type(crate): summary` describing the
whole change, e.g. `fix(ui): keep wheel scroll inside nested lists`.

Body template:

```markdown
## What / Why
<one short paragraph: the user-visible change and the motivation>

## Changes
- <one line per commit, from `git log origin/main..HEAD --oneline`>

## Verification
- <commands actually run this session and their results>
- <checks skipped, and why>

## Changelog
- <"none (internal)" or a one-line candidate for site/changelog.html at release time>
```

## Checks / merge

- Report `gh pr checks` results; GitHub CI may differ from local runs.
- If asked to merge: confirm the exact command first —
  `gh pr merge <N> --squash --delete-branch` unless the user specifies
  otherwise — and note that a release bump/changelog commit still happens
  separately on `main` per the release workflow.
