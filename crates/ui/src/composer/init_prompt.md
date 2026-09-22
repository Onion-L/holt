Generate or update an AGENTS.md file that gives future agent sessions the context they need to work in this repository.

Work in this order:

1. Check whether AGENTS.md already exists at the repository root. If it does, improve it in place — keep what is accurate, fix what is stale, add what is missing. Do not rewrite it wholesale.
2. Read the README, the manifests and lockfiles (Cargo.toml, package.json, go.mod, pyproject.toml, …), the CI configuration, and any existing instruction files (CLAUDE.md, .cursor/rules/, .github/copilot-instructions.md, CONTRIBUTING.md). Read source files only where those leave questions open.

What belongs in the file — each line should pass the test "would an agent likely miss this or get it wrong without help?":

- Exact build, test, lint, and run commands, including how to run a single test.
- The big-picture architecture: crate/module boundaries and data flow that only show up across multiple files.
- Non-obvious conventions: commit message format, code generation steps, required toolchain versions.

Leave out anything discoverable at a glance — directory trees, dependency lists, generic advice such as "write clean code" or "handle errors" — and anything you had to guess. When in doubt, omit.

Keep the file under 60 lines. Write it to AGENTS.md at the repository root (the current working directory when this is not a git repository). If the repository holds nothing worth recording, say so instead of writing a hollow file.
