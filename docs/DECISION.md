# Decision log

Every load-bearing decision, one entry each: the decision, the alternatives
rejected, and the commit(s) that landed it. `git show <sha>` restores the
full context. Current behavior lives in the [subsystem pages](subsystems/README.md);
this file only answers *why* — never *how it works today*.

Anchors are the entry slugs: `ADR-<slug>` citations in code and docs
resolve by searching this file. Supersession is an inline line on the old
entry; a changed decision appends a new entry, never edits an old one.

A decision earns an entry only if it names a rejected alternative, spans
modules or a serialized format, or someone might plausibly relitigate it.
Everything else belongs in a code comment.

## Index

Entries by the subsystem page they constrain. Read the row for your
area before changing code; a new entry adds its slug to its page's row
in the same commit.

| Page | Entries |
| --- | --- |
| [run-loop](subsystems/run-loop.md) | [turn-completion-is-an-engine-owned-event](#turn-completion-is-an-engine-owned-event) · [agent-core-is-a-holt-owned-workspace-crate](#agent-core-is-a-holt-owned-workspace-crate) |
| [persistence](subsystems/persistence.md) | [history-is-a-separate-persisted-record](#history-is-a-separate-persisted-record) · [transcript-append-only-log-per-chat-lock](#transcript-append-only-log-per-chat-lock) |
| [toolset-mechanics](subsystems/toolset-mechanics.md) | [in-process-ripgrep-for-content-search](#in-process-ripgrep-for-content-search) · [web-tools-full-text-ungated-pluggable-search](#web-tools-full-text-ungated-pluggable-search) · [agents-read-chats-through-validated-chat-links](#agents-read-chats-through-validated-chat-links) · [mcp-servers-join-as-engine-clients-behind-the-gate](#mcp-servers-join-as-engine-clients-behind-the-gate) · [mcp-tool-schemas-defer-through-a-loader](#mcp-tool-schemas-defer-through-a-loader) · [todo-card-is-the-record](#todo-card-is-the-record) |
| [provider-transport](subsystems/provider-transport.md) | [three-layer-provider-catalog](#three-layer-provider-catalog) |
| [subagents](subsystems/subagents.md) | [subagents-use-independent-history-and-foreground-results](#subagents-use-independent-history-and-foreground-results) |
| [prompts-skills](subsystems/prompts-skills.md) | [skills-referenced-in-place-from-standard-roots](#skills-referenced-in-place-from-standard-roots) · [metadata-only-skill-advertising](#metadata-only-skill-advertising) · [skill-invocation-is-an-inline-mention](#skill-invocation-is-an-inline-mention) |
| [permission-gate](subsystems/permission-gate.md) | [permission-modes-gate-mutations-no-os-sandbox](#permission-modes-gate-mutations-no-os-sandbox) |
| [modes](subsystems/modes.md) | [plan-mode-is-an-independent-approval-checkpoint](#plan-mode-is-an-independent-approval-checkpoint) · [model-setup-applies-stored-proposals](#model-setup-applies-stored-proposals) · [model-setup-scoped-chat-agent-never-applies](#model-setup-scoped-chat-agent-never-applies) · [api-keys-collected-through-key-requests](#api-keys-collected-through-key-requests) · [provider-mode-is-a-chat-mode](#provider-mode-is-a-chat-mode) |
| [goals-routines](subsystems/goals-routines.md) | [routines-fire-in-process-and-coalesce-missed-fires](#routines-fire-in-process-and-coalesce-missed-fires) · [goal-mode-verified-loop-at-turn-settle](#goal-mode-verified-loop-at-turn-settle) |
| [conversation-policy](subsystems/conversation-policy.md) | [attended-sends-run-through-a-paused-queue](#attended-sends-run-through-a-paused-queue) · [steer-starts-a-new-turn](#steer-starts-a-new-turn) · [last-message-edit-restarts-a-turn](#last-message-edit-restarts-a-turn) · [compaction-is-not-a-turn](#compaction-is-not-a-turn) · [automatic-titles-run-outside-pi-hooks](#automatic-titles-run-outside-pi-hooks) · [ask-user-is-a-question-card](#ask-user-is-a-question-card) |
| [terminals](subsystems/terminals.md) | [terminals-belong-to-chats-and-end-with-the-app](#terminals-belong-to-chats-and-end-with-the-app) |
| [workspace-git](subsystems/workspace-git.md) | [git2-as-the-git-backend](#git2-as-the-git-backend) · [git-hangs-off-spaces-no-repo-registry](#git-hangs-off-spaces-no-repo-registry) · [branch-is-live-working-tree-state](#branch-is-live-working-tree-state) · [ui-driven-git-writes-agent-surface-stays-read-only](#ui-driven-git-writes-agent-surface-stays-read-only) · [turn-diff-net-change-baseline](#turn-diff-net-change-baseline) · [turn-change-sets-and-review](#turn-change-sets-and-review) · [turn-restore-from-change-records](#turn-restore-from-change-records) · [session-worktrees-are-chat-owned-isolation](#session-worktrees-are-chat-owned-isolation) |
| [workspace-files](subsystems/workspace-files.md) | [file-editing-state-belongs-to-spaces](#file-editing-state-belongs-to-spaces) |
| [shell-registry](subsystems/shell-registry.md) | [home-space-for-project-less-chats](#home-space-for-project-less-chats) |
| UI, no page | [own-send-holds-in-place-when-there-is-reply-room](#own-send-holds-in-place-when-there-is-reply-room) · [main-transcript-is-top-anchored](#main-transcript-is-top-anchored) · [nested-scroll-containers-isolate-wheel-events](#nested-scroll-containers-isolate-wheel-events) · [urls-wrap-after-slashes-never-before](#urls-wrap-after-slashes-never-before) |

---

## git2-as-the-git-backend

`38d0858`. The git slice runs on `git2`. Rejected: `gix` (API churn across a
very large surface) and shelling to system `git` (install dependency,
porcelain-parsing fragility, version drift). The workspace's first C
dependency is deliberate — a crates.io build, not another `third_party`
snapshot. Swapping the backend later re-opens checksum and patch semantics.

## git-hangs-off-spaces-no-repo-registry

`38d0858`. Git serves the folders of spaces; there is no repo registry
(`ListRepos`/`AddRepo`/`CloneRepo`/`CreateRepo` stay unserved by design).
Checkout identity is `sha256(deviceId ‖ NUL ‖ git_dir)`. A parallel registry
would duplicate what spaces already model and re-open identity questions.

## turn-diff-net-change-baseline

`38d0858`. The turn diff is the net working-tree change since the chat's
last Turn started, against a `(HEAD sha, uncommitted patch)` baseline.
Rejected: HEAD-only (misattributes pre-existing dirt to the Turn) and a
full workdir snapshot (juggles `.git/index` while agent tools write
concurrently).

## own-send-holds-in-place-when-there-is-reply-room

`29fbd35`. A send holds the prompt in place when a third of the viewport is
free below; the glide to the top is the fallback. Rejected: the
unconditional pull (motion with no information reads as a flash) and
dropping the anchor (would fork the send path into two streaming
mechanisms). Holding is the degenerate glide.

## in-process-ripgrep-for-content-search

`6132ce3`. Content search runs ripgrep's crates in process. Rejected: a
system `rg` binary (install dependency, version drift — same cause as the
git2 decision) and hand-rolling on `regex` (re-implements
`grep-searcher`).

## main-transcript-is-top-anchored

`6a4e727`. The transcript lays out document-style from the top. Rejected:
keeping Bottom and fixing the load flicker (the flicker was the anchoring
model; Bottom forces a glue-management layer that exists only to fight
gpui's re-glue sentinel) and deleting the spring/glide machinery with the
flip (it is alignment-agnostic and deliberately kept).

## skills-referenced-in-place-from-standard-roots

`8d00fbb`. The filesystem is the skill registry: three standard roots,
nearest-root-wins, rescanned on every use. Rejected: copy-on-import (goes
stale, forfeits the `~/.agents/skills` interop point) and a registration
surface (a convention-based catalog needs no persistence or lifecycle UI) —
the same stance as no-repo-registry.

## metadata-only-skill-advertising

`8d00fbb`. Skills advertise metadata only; full content arrives by the model
reading `SKILL.md` through the read tool. Rejected: kimi's dedicated skill
tool with host-side injection (adds a tool and a mid-run steer pipeline)
and codex's developer-message placement (no such channel; holt rebuilds
context every run anyway). Manual trigger amended by
`skill-invocation-is-an-inline-mention` (`2a5c7a4`).

## branch-is-live-working-tree-state

`ce82eeb`. A branch switch is allowed at any time, including mid-Turn; the
local repo is the single source of truth. Rejected: deferring the switch
while a Turn runs ("a Turn is a Turn"), a pending-intent model
(half-effective states), and mid-session cwd moves. A turn-diff spanning a
mid-Turn switch shows the branch delta — accepted quirk.

## nested-scroll-containers-isolate-wheel-events

`02b825a`. Scrollables nested in a transcript `List` must `.occlude()`; a
bubble-phase wheel handler is insufficient because the outer listener may
run first. The unabsorbed remainder chains to the outer list at the scroll
boundary.

## automatic-titles-run-outside-pi-hooks

`9b20de6`. The title is a one-shot engine-owned background task, not a
pi-core hook (the harness hook registry did not exist). Buys: a separately
configured model, parallel execution, failure without touching the Turn.

## history-is-a-separate-persisted-record

`d972e67`. The model-facing History is its own append-only JSONL, appended
per message. Rejected: rebuilding from the Transcript on load (display
shapes are lossy; remembering something other than what the user sees is
worse than visibly remembering nothing), reusing pi's session codec (a
tree-shaped model holt does not use), and per-Turn writes (a crash mid-Turn
would lose completed tool results). Holt repairs on write because pi's
request-time repair discards aborted assistant messages outright.

## compaction-is-not-a-turn

`d972e67`, amended `9cbfa4e`. Compaction runs before a Turn, between tool
rounds, and after an overflow — never as a Turn. Rejected: turn-start-only
(overflow hits mid tool-loop), `transform_context` (cannot mutate the
loop's context), and modeling compaction as a Turn (would reset turn-scoped
state). In-Turn compact-and-continue was first rejected, then adopted
(`9cbfa4e`): ending the Turn made the user re-trigger what the engine
already knew it owed.

## permission-modes-gate-mutations-no-os-sandbox

`98a75a0`. Mutating calls meet a gatekeeper per the chat's mode; the
predicate is tool identity only. Real OS sandboxing was evaluated (Seatbelt,
Landlock+seccomp) and rejected: it could only confine bash, its
network-blocked profiles break cargo/npm, and it needs a per-OS kernel
story. No read-only tier and no path confinement — every mutating call
meets the gatekeeper regardless of target. A sandbox can slot beneath the
modes later as enforcement.

## steer-starts-a-new-turn

`c8ea119`. Steer interrupts the running Turn, waits for cleanup, and starts
a new Turn — explicit interruption over pi's loop-boundary message
injection, which cannot stop ongoing work and would bypass holt's per-Turn
transcript, repair, and diff semantics.

## subagents-use-independent-history-and-foreground-results

`c282635`. Children start from a brief, not the parent's History; the final
summary is the tool result. Background execution and follow-up
instructions are out of scope; the parent owns child execution, and there
are no automatic worktrees, merging, or rollback.

## terminals-belong-to-chats-and-end-with-the-app

`dcbe375`. Terminals belong to chats (not spaces), live only as long as the
app (no background host, no restoration), and any ending close requires
confirmation. Independent of the agent's bash tool.

## agents-read-chats-through-validated-chat-links

`0e809d0`. Cross-chat reads go through `read_chat` with validated links,
returning only transcript-visible text — never the model-facing History
(internal tool traffic; results would differ after compaction). Chat
content is untrusted data, not instructions.

## turn-completion-is-an-engine-owned-event

`8a8ecfd`. Turn completion is a typed engine-owned event published only
after durable settlement, live-only — one stable identity for any future
consumer, instead of each consumer inferring completion from session
snapshots.

## file-editing-state-belongs-to-spaces

`b0efe94`. File tabs, unsaved contents, and tree expansion belong to the
Space and are shared by its chats — per-chat drafts would let two chats
edit the same file blind to each other. Sharing the tab presentation does
not transfer ownership of the other views (the terminals decision is the
deliberate contrast).

## ui-driven-git-writes-agent-surface-stays-read-only

`5adc754`. Partially superseded: the UI write trio (stage/unstage/commit)
was removed. What stands: the agent's git surface stays read-only — wiring
git writes into the agent toolset must be an explicit future decision.

## attended-sends-run-through-a-paused-queue

`e15d06a`. The pause after a failure is not absolute: a first acceptance
while the channel is settled runs alone through a one-shot persisted grant
bound to the message id. Before it, one transient failure cost one manual
click per message, indefinitely.

## web-tools-full-text-ungated-pluggable-search

`4e0165b`, amended `95fa5e8`, `3bb8c3a`. Full text, never a model summary
(no hidden second model). Search backends are the user's choice —
provider-locked server-side search rejected (holt is multi-provider).
Neither tool enters the gate: fetch is a read, and the exfiltration channel
it leaves open is answered by domain rules, not per-call approval theater;
no SSRF filter either (localhost is a first-class use; bash already reaches
the network). Amendments: keyless Exa is active before any setup and off
persists (`95fa5e8`); user-defined backends describe the service's own
shape (a request template + JSON Pointers, or one MCP tool) in a separate
keyless file that never fails startup (`3bb8c3a`). Survey: docs/research/web-tools.md (five-way comparison —
Claude Code, Codex, opencode, Kimi, pi).

## turn-change-sets-and-review

`6cf767c`, amended `68c4ab2`. A settled Turn's change set is an immutable
review record: baseline after admission, per-file before/after persisted,
failed and interrupted Turns keep theirs. Amendment: a last-message edit
retracts the pruned Turn's record with it — the replacement Turn reruns
under the same message id from a baseline that already contains the work.

## plan-mode-is-an-independent-approval-checkpoint

`cc2dce5`, amended `cefaabb`, `34196f4`. Planning approval is a checkpoint
orthogonal to the permission mode, enforced by the toolset not the prompt.
v1 (plan documents on disk plus write/submit tools) was dropped when the
design was aligned with Codex's plan mode (`cefaabb`); submission then
became the structured `submit_plan` tool with the text block as fallback
(`34196f4`). The enforced read-only set and approval cards held throughout.

## model-setup-applies-stored-proposals

`81dbb27`. Superseded by `model-setup-scoped-chat-agent-never-applies`, then
`provider-mode-is-a-chat-mode`. What carries over: proposals are stored and
applied by id (never arguments at apply time), the apply forces human
approval no mode exempts, and keys never enter a chat.

## model-setup-scoped-chat-agent-never-applies

`81dbb27`. Superseded by `provider-mode-is-a-chat-mode` (`b521ef8`): the
hidden setup chat read as a chat squeezed into a modal. The scoping
argument carried over — catalog tools mount only where catalog setup runs;
normal chats' catalog-write capability is nil.

## three-layer-provider-catalog

`81dbb27`. Compiled under the hand-edited overlay under the live layer;
programmatic writes touch only the live layer. Rejected: the tool writing
`provider-store.json` — restart-gated, the first programmatic writer of a
0600 user-owned file, and it worsens the snapshot-freeze problem.

## api-keys-collected-through-key-requests

`6099d5f`, amended `933230a`. Keys arrive through request cards, never as
chat text; the value goes straight to the credential store and entering it
approves the exact destination shown (a changed baseUrl re-arms the
request). Amended by provider-mode: the card lives in the transcript and
approvals persist with the chat.

## last-message-edit-restarts-a-turn

`8308b5f`. Only the latest user message is editable; submission cancels the
active Turn, prunes that message's later transcript and History, preserves
the identity, and reruns with current settings. Existing side effects and
usage are retained — editing changes conversation state, not the working
tree or accounting.

## transcript-append-only-log-per-chat-lock

`f8fcced`, amended `671f35c`. The transcript is an append-only JSONL behind
a per-chat lock. Before: a whole-file snapshot on a 120 ms tick — O(session)
per publish, quadratic growth (measured ≈130× on a real session: 85 MB for
a 647 KB entry), and one runtime-wide lock serializing every chat's
persistence. The full entry line stays the replay's self-heal anchor;
streaming landings became incremental `parts`/`part` lines (linear) in
`671f35c`.

## mcp-servers-join-as-engine-clients-behind-the-gate

`306218e`. The engine speaks MCP as a client; every `mcp__` call is presumed
mutating by prefix. Rejected: trusting `readOnlyHint` (the spec calls
annotations untrusted) and never gating (foreign code with zero audit
surface). Lazy lifecycle, no background restart loop; stdio children get a
sanitized environment (a Dock-launched app inherits launchd's PATH, which
cannot resolve `npx` at all). Survey: docs/research/mcp.md.

## skill-invocation-is-an-inline-mention

`2a5c7a4`. Invocation is an inline `$` mention in an ordinary message — one
path means one parser, one error story, one queue semantics; a mention is
part of a message, so staging/editing apply uniformly. The linked form is
path-authoritative and bypasses shadowing. Rejected: keeping the exclusive
`/skill` queue item alongside mentions, and name-only resolution (the path
disambiguates same-name skills).

## provider-mode-is-a-chat-mode

`b521ef8`, amended `06b8a00`. Catalog setup is a chat mode, not a dialog.
Rejected: the dialog (its problems were structural), a form-first flow with
AI autofill (brings back the form the feature exists to avoid), mounting
catalog tools in every chat (prompt-injection surface), and in-memory
proposals (cards in a persisted history would dead-end after a restart).
The staleness gate fingerprints only the touched providers — a
whole-catalog baseline would stale every card on any other write.
Amendment: mode entry/exit moved to `/provider` and `/plan` directives with
a top-right state card (`06b8a00`).

## mcp-tool-schemas-defer-through-a-loader

`d637cd6`. MCP schemas leave static context and load through a loader tool
whose synthetic pair marks the Turn's snapshot. Rejected: a per-provider
conditional bootstrap, a static catalog in the system prompt (busts the
cache prefix on every config change), and a per-tool deferral flag in
`mcp.json` (user-tunable fragmentation of a harness invariant). Survey:
docs/research/mcp.md.

## session-worktrees-are-chat-owned-isolation

`1cebfab`. The worktree intent is chat state absorbed at command acceptance
(a crash cannot drop it), materialized at admission, joining as its own
Space. `CreateWorktree`/`DeleteWorktree` stay unserved — lifecycle beyond
creation-on-demand is a later slice.

## home-space-for-project-less-chats

`4068bf0`. There is no workspace-less conversation: an engine-owned Home
space absorbs project-less chats, so `~` is never a cwd again (the project
skill root collided with the personal root there). `/init` is not offered
in Home — bootstrapping an AGENTS.md is a project act.

## ask-user-is-a-question-card

`d6f4c13`. A question is a non-blocking card that stops the Turn; the
answers arrive as the next user message. Rejected: a blocking tool (the
permission vocabulary answers "may I?", not arbitrary content, and an
in-memory block goes stale across restarts). The card state rides the doc,
so it is restart-safe by construction.

## turn-restore-from-change-records

`032c8ac`. Restore writes a record's old content back by plain file I/O —
no Git — gated per file on the disk still hashing to the record. A file
that moved on is refused, never merged.

## routines-fire-in-process-and-coalesce-missed-fires

`5d75c7e`. The scheduler is an engine task alive exactly as long as the
process (no daemon — whether holt should grow one stays open). Missed fires
coalesce into one catch-up run; one live run per routine; a blocked run
waits for the human with no gate timeout.

## agent-core-is-a-holt-owned-workspace-crate

`9bda72d`. The agent core is a workspace crate with its history subtrees in
— a core fix and its consumer land in one commit. Upstream pi is no longer
tracked (goldens pin behavior); the crate keeps MIT and stays free of holt
domain concepts; tests link as one binary and never touch paid APIs or the
developer's environment (`PI_TEST_LIVE` opts in — a deliberate deviation
from upstream's skipIf).

## goal-mode-verified-loop-at-turn-settle

`bd5e9ae`, amended `179cbca`. The verifier runs inside the settled Turn's
driver iteration, judged from evidence (tool calls, results, the frozen
change set) — never the model's own plans; a bare COMPLETE without evidence
is no verdict. The continuation is an ordinary visible queue item — a
hidden channel was rejected because holt's queue is durable and
user-editable. A `MessagePart` variant was rejected (a Notice row already
expresses it). Verifier usage books immediately: the Turn's batch settles
before the verifier runs. `179cbca` bounds the pass (120 s wall clock — the
HTTP layer has no total timeout and Stop cannot reach the token) and pauses
on an un-enqueueable continuation.

## urls-wrap-after-slashes-never-before

`d781c50` (issue #44). The vendored gpui wrap rule breaks
after `/` and never before it, in all three copies of the candidate rule
(`LineWrapper::wrap_line`, `LineLayout::compute_wrap_boundaries`, and
`truncate_wrapped_line`) and in their no-candidate hard-break fallbacks:
inside a URL the only `/` preceded by a non-word char is the `://` pair,
so the old rule made the scheme the URL's single break point and greedy
wrapping always split it (`https:/` + `/open…`) — and the fallback could
land on the second `/` just the same. Rejected: holt-side softening of
the chip error text (ZWSP break hints pollute copied text, and every
other wrapped surface — markdown prose, `line_clamp` state cards — kept
the bug), and making `://` unbreakable alone (a URL then has no break
candidate before its first path slash and hard-breaks mid-hostname).
Matches browser behavior: break after the solidus, never before it.

## todo-card-is-the-record

(issue #47; landing sha filled on merge.) The `todo` tool is stateless —
every call carries the whole list — and the Turn's one `TodoCard` part,
upserted in place from the result details, is the record (restart-safe by
riding the transcript, like the question card). Rejected: engine-held
chat-scoped state (a second store to persist and reconcile, when the card
already is it); reviving the vestigial ACP `ToolCall::Todo` chip as the
checklist surface (a chip renders one line per call — a card that rewrites
in place is a part, not a chip; the chip stays as the per-call record);
mounting beyond main-chat Turns (the MCP policy — subagent, planning, and
Provider Mode Turns keep their curated toolsets).
