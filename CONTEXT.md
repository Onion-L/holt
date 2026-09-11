# Holt Domain Glossary

- **Workspace registry**: typed index of devices, spaces, chats, and session status rows.
- **Watch**: a live engine stream that keeps one UI snapshot current.
- **Watch coordinator**: the internal module that owns watch subscription, frame decoding, retry, and cancellation policy.
- **Workspace registry adapter**: a storage-specific implementation of the workspace registry interface, such as Loro or HLC/overlay storage.
- **Space**: a synced (device, folder) pair — the unit of organization in the sidebar. A space's folder may or may not be a git work tree.
- **Chat link**: a `holt://` reference to one Chat in a specific Workspace. Its opaque Workspace locator prevents the link from resolving against a different local or synced Workspace.
  _Avoid_: conversation link, session link
- **Checkout**: the canonical identity of one working copy of a git work tree, `sha256(deviceId ‖ NUL ‖ git_dir)`. Diffs are grouped per checkout, not per folder path.
- **Turn**: one agent run of a chat — starts when its queued command begins running, ends when the run finishes or is interrupted.
- **Pending message**: a user message or skill invocation accepted for later execution in its chat. Each pending message starts its own Turn when its turn to run arrives.
- **Message queue**: a chat's ordered collection of pending messages and manual Compaction commands; ordinary sends join its tail. Pending items survive an app restart, after which execution is paused until the user explicitly continues the queue.
- **Paused queue**: a message queue whose pending items do not start automatically; an Attended send is the one exception and is admitted at once. Continue resumes automatic execution and lifts any single-message restriction; Run now executes a selected message while leaving the rest paused. When the queue settles with no pending items, nothing executing, and no queue-level error, the pause is cleared.
- **Attended send**: a message the user submits while nothing executes in its chat. Even a paused queue admits it immediately, ahead of parked items, and it runs alone: it neither waits behind parked work nor resumes it, and when it settles the queue is as paused as before. A submission that arrives while work is executing is an ordinary pending item.
  _Avoid_: direct send (nothing bypasses the queue)
- **Steer**: an explicit request to interrupt current work and prioritize a pending message for a new Turn after that work has ended. Multiple Steer requests awaiting execution keep their submission order ahead of ordinary pending items; an interrupted Turn stays in the record and is not automatically retried.
- **Working directory** (of a chat): the folder each of the chat's Turns runs in — the space's folder. It is fixed when the chat is created and never moves during the chat's life; only the branch inside it switches. A Turn always runs on whatever the working directory holds when the Turn starts.
- **External application opener**: the titlebar control that opens the current Chat's Working directory in a user-selected local application. The device remembers the selection and defaults to Zed when available.
- **File sidebar**: the file tree rooted at the current Chat's Working directory, shown to the right of the file contents area. It follows the selected Chat's directory and can be hidden independently of the contents area.
- **File editing state**: a Space's open file tabs, unsaved file contents, and file-tree expansion state, shared by all Chats in that Space.
- **Branch switch**: moving a working directory's HEAD to another ref (a safe `git switch` — it never discards, merges, or stashes). A switch may happen while a Turn is live: the Turn is not interrupted, and the next Turn runs on whatever the working directory holds at its start. A ref already checked out in another worktree cannot be switched to — that worktree is reached by importing it as a Space, never by pointing a chat at it.
- **Worktree**: a linked git work tree. It is a Space-level concept: a worktree joins holt by being imported as its own Space, and chats under it switch branches inside it like any other working directory.
- **Source context**: the repository identity a chat's Turn is stamped with when it starts — repo root, branch, checkout id. It is the only branch metadata trusted for identity; the chat's scalar branch field just echoes the most recent Turn's stamp.
- **Diff scope**: which comparison a Changes pane shows. Four flavors: **working tree** (uncommitted changes vs HEAD), **branch** (everything the branch adds over the merge-base with a base ref, working tree included), **latest turn** (net changes since the chat's last turn started), **history** (the commit graph); a fifth, **commit**, exists only as a pinned per-commit pane.
- **Base ref**: the branch a branch diff is taken against; defaults to the repo's default branch.
- **Turn diff**: the net working-tree changes since the chat's last turn started. Pre-existing uncommitted changes are not part of a turn unless the turn touched them.
- **Turn baseline**: the working-tree state captured immediately after a Turn is admitted and before its execution begins. It defines the comparison boundary for that Turn's change set.
- **Turn change set**: the net file changes produced between a Turn baseline and its current or final working-tree state. It belongs to the parent main-chat Turn; subagent edits are included in the parent's set.
- **File change**: one added, modified, deleted, or rename-detected path within a Turn change set, with Git-derived line counts where applicable.
- **Review**: a read-only view of a Turn change set's per-file diff in the right sidebar.
- **Open**: opening the current post-Turn file in the right sidebar; deleted files remain reviewable through their diff.
- **Git panel**: the right-pane panel for a working directory's uncommitted git state. It lists changed files in three sections — Staged, Unstaged, Untracked — and lets the user stage, unstage, and commit files; its History tab shows the commit graph. The Changes diff viewer stays a separate surface.
  _Avoid_: calling the status list "Changes" (Changes is the Diff scope viewer)
- **Staging**: git's index as operated from the Git panel. Staging marks a path's current changes for the next commit; unstaging removes them from the index. Neither touches file contents. A path can carry staged and unstaged changes at once, and then appears in both sections. A conflicted path can be neither staged nor unstaged from the panel. Ignored paths never appear.
  _Avoid_: tracked (the middle section is Unstaged), checkbox state as a source of truth (the index is)
- **Content search**: regex search over file contents, rooted at the chat's working directory by default and scoped by a path prefix and a filename filter. Relative paths resolve against that directory; absolute paths may select another search root. The agent-facing tool is named `grep`; the transcript renders it as a Search chip. Hidden files are searched; ignored files are not.
- **Web fetch**: the agent-facing retrieval of one web URL over http(s), returned as readable content within the tool's size limits. It reads pages; finding them is Web search's job.
  _Avoid_: fetch (that is git vocabulary), browse
- **Web search**: the agent-facing query for web results — a list of titles, URLs, and snippets from the configured Search backend. It finds pages; Web fetch reads them.
  _Avoid_: search on its own (Content search is the local one)
- **Search backend**: the external search service Web search queries, chosen by the user in Settings and carrying its own key. When none is configured, the Web search tool is absent from the model's toolset — never an error.
  _Avoid_: search provider (a Provider is a model provider)
- **Path reference**: a file or folder location attached to a user message for the agent to consult. It refers to whatever exists at that location when the agent reads it, including a missing target; it does not preserve a snapshot of the contents.
- **Managed image**: a local image owned by Holt, created from pasted image content that has no source file path. After submission it is retained with its chat so a Path reference can still preview or read it after restart.
- **Slash command**: a composer directive starting with `/` that the UI intercepts and handles itself — sent as a typed command (`/skill`, `/compact`) or answered locally, never as prompt text.
- **Skill**: an instruction bundle in the Agents Skills format — a directory holding `SKILL.md` (YAML frontmatter with `name` and `description`) plus optional resources — that extends the agent's behavior. Holt references skills where they live; it never copies, moves, or registers them. A skill becomes available by being placed in a skill root — that is the only way in.
- **Skill root**: a directory holt scans for skills. Three, in precedence order when names collide: **project** (`.agents/skills` at the chat's working directory) > **personal** (`~/.agents/skills`) > **holt** (`~/.holt/skills`). The nearest root wins; shadowed and invalid skills surface only in Settings, never in the composer's skill menu.
- **Skill listing**: the model-visible advertisement of available skills — name, description, and location only, never content. Skills marked `disable-model-invocation` are excluded from it but stay manually invocable.
- **Skill invocation**: handing a skill's full content to the agent. The model does it itself by reading the skill file once the listing matches its task; the user forces it with the `/skill` slash command, which starts a Turn whose prompt is the skill's formatted content plus any extra instructions.
- **Transcript**: the scrollable rendered history of a chat — user messages, agent output, and tool rows in order. It only ever grows; Compaction never removes rows from it. It is anchored document-style: rows lay out from the top of the pane, and a short transcript leaves empty space below rather than rising from the bottom.
- **History**: an agent's model-facing message sequence — what it sends to the model as prior conversation. A chat and each of its Subagents have separate Histories. Persisted alongside the Transcript, but a separate record: Compaction shrinks the History, never the Transcript.
- **Compaction**: replacing the older part of an agent's History with a model-written summary while keeping a recent tail verbatim. Triggered automatically when the History approaches the model's context window; a chat also supports manual `/compact`. Marked in the Transcript by a divider whose summary can be expanded.
- **Pinned** (a transcript): the state in which the viewport follows the tail as new rows stream in. Scrolling up releases the pin; scrolling back near the tail re-engages it.
- **Saved viewport**: a chat's remembered scroll position, restored when the chat is reopened. A chat with no saved viewport opens at its latest content.
- **Title source**: whether a chat title was supplied by the user or produced automatically; a user-supplied title is authoritative over later automatic suggestions.
- **Automatic title**: a short, model-generated name derived from a chat's first user prompt, used only when the chat has no user-supplied title.
- **Title settings**: the device-wide choice of model and instruction used for automatic titles; an empty model means automatic titles are disabled.
- **Title task**: the one-shot background operation that asks the configured model for an automatic title after a chat receives its first user prompt; it is independent of the Turn lifecycle and never becomes chat History.
- **Terminal**: an interactive shell session owned by one Chat and operated directly by the user inside Holt; switching chats or hiding its view keeps it running. It is independent of the agent's bash tool calls and does not automatically contribute to History or Transcript.
- **Subagent**: an agent delegated a bounded task by a parent agent, with its own History. It receives a Task brief and applicable project instructions, and returns a final summary to its waiting parent; several Subagents may work in parallel.
- **Task brief**: the goal, necessary background, and acceptance criteria a parent agent supplies to a Subagent. It does not include the parent's entire History.
- **Explorer**: a Subagent that investigates and reports findings using only reading, Content search, another Chat's Transcript, and the web tools; it cannot change files or execute commands.
- **Worker**: a Subagent that can change files and execute commands under its parent Turn's Permission mode.
- **Permission mode**: the per-chat standing policy that stands between a Turn's mutating tool calls and execution — which gatekeeper judges each one: the user (confirm changes), a model review pass (auto-review), or none (full access). Reads are never gated. New chats inherit the last mode used on the device.
- **Approval**: a mutating tool call paused for the user's verdict in confirm-changes mode — allow once, always-allow, deny, or deny with a written note (the note becomes the reason the model sees). Rendered as an approval chip in the Transcript that settles to its verdict.
- **Auto-review**: the model pass that judges each mutating tool call before execution in auto-review mode — same model as the chat; a rejection blocks the call and returns the reason to the agent. Rendered as review chips in the Transcript.
- **Always-allow**: a chat-scoped, in-memory grant that passes matching mutating calls through the gate for the rest of the app session — bash matches by command prefix, write/edit by exact file path. Checked before the gatekeeper, so it holds across mode switches; cleared on restart and never persisted.
_Avoid_: sandbox (nothing is OS-sandboxed), trust level, ACL

- **Plan Mode**: a chat state in which the agent may inspect and reason about the workspace, write a pending plan document, and request approval before implementation; it is independent of the chat's Permission mode.
_Avoid_: planning permission mode, read-only permission mode
- **Plan document**: a versioned Markdown document under the chat's working directory at `.holt/plans`, representing one proposed implementation plan.
_Avoid_: todo list, task list
- **Plan submission**: the explicit runtime action that submits the active Plan document for user approval.
_Avoid_: plan text, plan output
- **Active plan**: the one plan document currently associated with a chat's Plan Mode state; a chat may retain older plans for audit.
_Avoid_: current todo
_Avoid_: permission prompt, confirm dialog (for Approval)
_Avoid_: auto-approve (Auto-review can reject)
_Avoid_: grep tool, agent search
_Avoid_: agent diff, session diff
_Avoid_: importing or registering a skill (placement in a skill root is the only way in)
_Avoid_: refs fixed at creation, locked session refs (a chat's working directory and branch are switchable at any time)
_Avoid_: context, conversation, memory (for the model-facing message sequence — it is the History)
_Avoid_: summarization, truncation, pruning (for shrinking the History — it is Compaction)
