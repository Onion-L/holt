# Holt Domain Glossary

- **Workspace registry**: typed index of devices, spaces, chats, and session status rows.
- **Watch**: a live engine stream that keeps one UI snapshot current.
- **Watch coordinator**: the internal module that owns watch subscription, frame decoding, retry, and cancellation policy.
- **Workspace registry adapter**: a storage-specific implementation of the workspace registry interface, such as Loro or HLC/overlay storage.
- **Space**: a synced (device, folder) pair — the unit of organization in the sidebar. A space's folder may or may not be a git work tree.
- **Checkout**: the canonical identity of one working copy of a git work tree, `sha256(deviceId ‖ NUL ‖ git_dir)`. Diffs are grouped per checkout, not per folder path.
- **Turn**: one agent run of a chat — starts when its queued command begins running, ends when the run finishes or is interrupted.
- **Diff scope**: which comparison a Changes pane shows. Four flavors: **working tree** (uncommitted changes vs HEAD), **branch** (everything the branch adds over the merge-base with a base ref, working tree included), **latest turn** (net changes since the chat's last turn started), **history** (the commit graph); a fifth, **commit**, exists only as a pinned per-commit pane.
- **Base ref**: the branch a branch diff is taken against; defaults to the repo's default branch.
- **Turn diff**: the net working-tree changes since the chat's last turn started. Pre-existing uncommitted changes are not part of a turn unless the turn touched them.
- **Content search**: regex search over file contents, rooted at the chat's working directory by default and scoped by a path prefix and a filename filter. Relative paths resolve against that directory; absolute paths may select another search root. The agent-facing tool is named `grep`; the transcript renders it as a Search chip. Hidden files are searched; ignored files are not.
- **Slash command**: a composer directive starting with `/` that the UI intercepts and handles itself. It never becomes a Command and never reaches the agent as prompt text.
- **Skill**: an instruction bundle in the Agents Skills format — a directory holding `SKILL.md` (YAML frontmatter with `name` and `description`) plus optional resources — that extends the agent's behavior. Holt references skills where they live; it never copies, moves, or registers them. A skill becomes available by being placed in a skill root — that is the only way in.
- **Skill root**: a directory holt scans for skills. Three, in precedence order when names collide: **project** (`.agents/skills` at the chat's working directory) > **personal** (`~/.agents/skills`) > **holt** (`~/.holt/skills`). The nearest root wins; shadowed and invalid skills surface only in Settings, never in the composer's skill menu.
- **Skill listing**: the model-visible advertisement of available skills — name, description, and location only, never content. Skills marked `disable-model-invocation` are excluded from it but stay manually invocable.
- **Skill invocation**: handing a skill's full content to the agent. The model does it itself by reading the skill file once the listing matches its task; the user forces it with the `/skill` slash command, which starts a Turn whose prompt is the skill's formatted content plus any extra instructions.
_Avoid_: grep tool, agent search
_Avoid_: agent diff, session diff
_Avoid_: importing or registering a skill (placement in a skill root is the only way in)
