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
| [run-loop](subsystems/run-loop.md) | [turn-completion-is-an-engine-owned-event](#turn-completion-is-an-engine-owned-event) · [agent-core-is-a-holt-owned-workspace-crate](#agent-core-is-a-holt-owned-workspace-crate) · [foreign-thinking-is-dropped-not-replayed-as-text](#foreign-thinking-is-dropped-not-replayed-as-text) · [turns-resume-from-an-intent-journal](#turns-resume-from-an-intent-journal) |
| [persistence](subsystems/persistence.md) | [history-is-a-separate-persisted-record](#history-is-a-separate-persisted-record) · [transcript-append-only-log-per-chat-lock](#transcript-append-only-log-per-chat-lock) · [turns-resume-from-an-intent-journal](#turns-resume-from-an-intent-journal) · [step-checkpoints-in-a-chat-owned-object-store](#step-checkpoints-in-a-chat-owned-object-store) |
| [toolset-mechanics](subsystems/toolset-mechanics.md) | [in-process-ripgrep-for-content-search](#in-process-ripgrep-for-content-search) · [web-tools-full-text-ungated-pluggable-search](#web-tools-full-text-ungated-pluggable-search) · [agents-read-chats-through-validated-chat-links](#agents-read-chats-through-validated-chat-links) · [mcp-servers-join-as-engine-clients-behind-the-gate](#mcp-servers-join-as-engine-clients-behind-the-gate) · [mcp-tool-schemas-defer-through-a-loader](#mcp-tool-schemas-defer-through-a-loader) · [todo-card-is-the-record](#todo-card-is-the-record) · [agent-commands-run-under-git-bash-on-windows](#agent-commands-run-under-git-bash-on-windows) · [addressed-path-vocabulary](#addressed-path-vocabulary) |
| [provider-transport](subsystems/provider-transport.md) | [three-layer-provider-catalog](#three-layer-provider-catalog) |
| [subagents](subsystems/subagents.md) | [subagents-use-independent-history-and-foreground-results](#subagents-use-independent-history-and-foreground-results) |
| [prompts-skills](subsystems/prompts-skills.md) | [skills-referenced-in-place-from-standard-roots](#skills-referenced-in-place-from-standard-roots) · [metadata-only-skill-advertising](#metadata-only-skill-advertising) · [skill-invocation-is-an-inline-mention](#skill-invocation-is-an-inline-mention) |
| [permission-gate](subsystems/permission-gate.md) | [permission-modes-gate-mutations-no-os-sandbox](#permission-modes-gate-mutations-no-os-sandbox) · [addressed-path-vocabulary](#addressed-path-vocabulary) |
| [modes](subsystems/modes.md) | [plan-mode-is-an-independent-approval-checkpoint](#plan-mode-is-an-independent-approval-checkpoint) · [model-setup-applies-stored-proposals](#model-setup-applies-stored-proposals) · [model-setup-scoped-chat-agent-never-applies](#model-setup-scoped-chat-agent-never-applies) · [api-keys-collected-through-key-requests](#api-keys-collected-through-key-requests) · [provider-mode-is-a-chat-mode](#provider-mode-is-a-chat-mode) |
| [goals-routines](subsystems/goals-routines.md) | [routines-fire-in-process-and-coalesce-missed-fires](#routines-fire-in-process-and-coalesce-missed-fires) · [goal-mode-verified-loop-at-turn-settle](#goal-mode-verified-loop-at-turn-settle) · [goal-card-drops-the-steps-checklist](#goal-card-drops-the-steps-checklist) |
| [conversation-policy](subsystems/conversation-policy.md) | [attended-sends-run-through-a-paused-queue](#attended-sends-run-through-a-paused-queue) · [steer-starts-a-new-turn](#steer-starts-a-new-turn) · [last-message-edit-restarts-a-turn](#last-message-edit-restarts-a-turn) · [compaction-is-not-a-turn](#compaction-is-not-a-turn) · [automatic-titles-run-outside-pi-hooks](#automatic-titles-run-outside-pi-hooks) · [ask-user-is-a-question-card](#ask-user-is-a-question-card) · [turns-resume-from-an-intent-journal](#turns-resume-from-an-intent-journal) · [rewind-to-a-step-is-all-or-nothing](#rewind-to-a-step-is-all-or-nothing) |
| [terminals](subsystems/terminals.md) | [terminals-belong-to-chats-and-end-with-the-app](#terminals-belong-to-chats-and-end-with-the-app) |
| [workspace-git](subsystems/workspace-git.md) | [git2-as-the-git-backend](#git2-as-the-git-backend) · [git-hangs-off-spaces-no-repo-registry](#git-hangs-off-spaces-no-repo-registry) · [branch-is-live-working-tree-state](#branch-is-live-working-tree-state) · [ui-driven-git-writes-agent-surface-stays-read-only](#ui-driven-git-writes-agent-surface-stays-read-only) · [turn-diff-net-change-baseline](#turn-diff-net-change-baseline) · [turn-change-sets-and-review](#turn-change-sets-and-review) · [turn-restore-from-change-records](#turn-restore-from-change-records) · [session-worktrees-are-chat-owned-isolation](#session-worktrees-are-chat-owned-isolation) · [step-checkpoints-in-a-chat-owned-object-store](#step-checkpoints-in-a-chat-owned-object-store) · [rewind-to-a-step-is-all-or-nothing](#rewind-to-a-step-is-all-or-nothing) · [step-checkpoints-are-bounded-per-chat](#step-checkpoints-are-bounded-per-chat) · [addressed-path-vocabulary](#addressed-path-vocabulary) |
| [workspace-files](subsystems/workspace-files.md) | [file-editing-state-belongs-to-spaces](#file-editing-state-belongs-to-spaces) · [addressed-path-vocabulary](#addressed-path-vocabulary) |
| [shell-registry](subsystems/shell-registry.md) | [home-space-for-project-less-chats](#home-space-for-project-less-chats) · [self-update-trust-is-per-platform](#self-update-trust-is-per-platform) |
| UI, no page | [own-send-holds-in-place-when-there-is-reply-room](#own-send-holds-in-place-when-there-is-reply-room) · [main-transcript-is-top-anchored](#main-transcript-is-top-anchored) · [nested-scroll-containers-isolate-wheel-events](#nested-scroll-containers-isolate-wheel-events) · [urls-wrap-after-slashes-never-before](#urls-wrap-after-slashes-never-before) · [ui-strings-live-in-a-compile-time-catalog](#ui-strings-live-in-a-compile-time-catalog) |

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

## foreign-thinking-is-dropped-not-replayed-as-text

`b57c49b`. When history is replayed to a different model,
`transform_messages` drops non-redacted foreign thinking blocks instead of
converting them to text. Rejected: pi-core's convert-to-text — observed
2026-10-16 after a mid-chat GLM→Kimi switch, ~120 replayed turns of
reasoning-as-text taught Kimi K3 to emit its own reasoning in the text
channel (0 thinking tokens, the whole reply as reasoning prose, so the UI
showed raw thinking with no fold); reproduced standalone by replaying the
same contaminated history to the kimi endpoint, while a clean history
returns proper thinking blocks. Claude Code likewise never replays foreign
thinking. Same-model thinking still replays, signature intact.

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

## goal-card-drops-the-steps-checklist

(issue #49). The goal card stopped rendering the verifier's steps as a
checklist, and the Turn's todo list moved into the card as a labeled
section — the standalone capsule stands down in goal mode, so two similar
cards never stack. The verifier is a judge, not a planner: its own prompt
disowns the `[ ]` half ("plans, intentions, and todo lists are not
progress"), and all-checked never implied COMPLETE — the verdict line is
the sole authority, so the checklist form promised a completion contract
that does not exist. The steps survive on the card only as a compact
`done/total verified` indicator. Rejected: keeping both lists sectioned
on one card (two look-alike checklists with different trust levels —
self-reported intent vs verified evidence — was the duplication being
fixed); restyling the todo capsule as a chromeless line (expanding still
produced a second card); merging the two lists into one (conflates intent
with evidence). The steps' per-item text belongs in the transcript as a
per-check verdict row — an engine change filed as a follow-up, since a
CONTINUE pass pushes no transcript row today.

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

`2dc0ce8` and `487b6a2` (issue #47). The `todo` tool is stateless —
every call carries the whole list — and the Turn's one `TodoCard` part,
upserted in place from the result details, is the record (restart-safe by
riding the transcript, like the question card). Rejected: engine-held
chat-scoped state (a second store to persist and reconcile, when the card
already is it); reviving the vestigial ACP `ToolCall::Todo` chip as the
checklist surface (a chip renders one line per call — a card that rewrites
in place is a part, not a chip; the chip stays as the per-call record);
mounting beyond main-chat Turns (the MCP policy — subagent, planning, and
Provider Mode Turns keep their curated toolsets).

## turns-resume-from-an-intent-journal

`52eb35d`, `45231e3`, `2fd3425`, amended `b5993ee` and `e73b1ce`. A Turn cut
off by a crash or quit resumes at launch from its last committed step instead
of being recovered as interrupted; continuing no longer needs a new queue
item. The journal is a new kind in the History JSONL (see
history-is-a-separate-persisted-record): once the permission gate lets a call
through, the engine fsyncs a tool intent before the call runs. On load, a call
without a result is answered by what the journal proves — no intent: not
executed; intent: started and may have partly run, check before repeating (the
old "interrupted by the user … not executed" was wrong for a crash). Nothing
is re-run automatically; the model decides. An unfinished model stream is
never continued: the request is re-sent from History. A Turn that crashes
three times in a row stops resuming and is recovered as interrupted, as is one
the user stopped: Stop, Steer and edit-last record the stop on the started
item before cancelling, so a quit that beats the settle cannot undo it. Core
is unchanged: the engine already owns the last hook before execution (the
gate), and resumption goes through `run_agent_loop_continue`. A hard kill runs
no destructor, so a bash process group outlives it and keeps changing the
workspace; launch kills every recorded group before any Turn resumes.
Resumption is independent of goal mode: goal is one consumer, not the
orchestration of long runs.
Rejected: asking before resuming on open (Stop is how a user ends a run; a
quit is not a decision about the Turn); pi-durable's model of one SQLite
transaction per step over task state machines (a second store beside History,
transcript and queue; Holt no longer builds on pi — its spec and source
informed the intent rule only); automatic re-run of read-only tools after a
crash (a per-tool replay policy saves one model round; deferred until that
round matters); adopting core's ported pi harness scaffold (`agent/harness`,
never wired, every operation `HarnessNotImplemented`, and its session codec
was already rejected); deterministic re-execution against a journal in the
Temporal/Restate style (an in-process loop with one model call per step gains
nothing from replaying code, and model and tool calls cannot be made
deterministic); resuming a half-received provider stream (no provider supports
it). Workspace snapshots per step are deferred to their own entry — they
reopen turn-diff-net-change-baseline.

## step-checkpoints-in-a-chat-owned-object-store

`f3d4a58`, `b69fbaa`. Each tool call that changes files in a Git working tree
records a step checkpoint: every changed path's exact before and after bytes,
as blobs in a bare Git object store owned by the chat
(`checkpoints/<chatId>/objects`), listed in an append-only step log beside it.
The store holds only what the agent's own calls changed: write and edit read
their target around the call; bash stores every uncommitted file's bytes
before the command (identical content is stored once) and takes clean files
from HEAD, so a step's before side is exact bytes — never content rebuilt from
patch text, which loses CRLF and a missing final newline. Files above 10 MiB,
and anything but a regular file, are recorded by path only. A non-Git working
tree records nothing, as its Turn change set answers `Unsupported`. Deleting a
chat deletes the directory; libgit2 has no gc, and per-chat stores need none.
Step ids in the log are opaque to every consumer, so the storage can change
behind them. While a call runs, its start (the before sides) waits on disk; a
crash leaves it there, and the next load measures it against the working tree,
records an interrupted step, and adds the changed files to the cut-off call's
"may have partly run" result — the model still decides, nothing is re-run (see
turns-resume-from-an-intent-journal). This does not reopen
turn-diff-net-change-baseline: the Turn diff keeps its baseline; checkpoints
are a separate per-step record.
Rejected: a Holt-specific blob format now (it rebuilds Git's content
addressing and compression before it can improve on them; revisit for chunked
dedup of large binaries, dedup across workspaces, non-Git folders, or
multi-agent branching); objects in the user's `.git` under a private ref
(grows the user's repository); a whole-tree snapshot per step (the `git
add`-everything disk blowups in OpenCode, Codex, Roo and Cline; libgit2 takes
120–660 ms per scan); APFS `clonefile` (cost grows with file count, ignored
trees included: 38 s on a 141 GB tree); borrowing the user's objects through
alternates (nothing unchanged is stored, and a user's history rewrite plus gc
could prune what a checkpoint points at).

## rewind-to-a-step-is-all-or-nothing

`10cfe6d`, `4b5447b`, `399608d`. A chat rewinds to just after any tool step of
a settled Turn, conversation and files together, in one action
(`RewindToStep`). The History is cut after the results of the assistant
message that made the call — parallel siblings stay, so no call is left
without its result. The files are put back from the step checkpoints (see
step-checkpoints-in-a-chat-owned-object-store): every step recorded after the
kept tail's newest timestamp, by a call the kept History does not hold, is
undone; subagent steps carry the child's call ids, so only the clock places
them. One file that moved on since its last undone step, whose content was
never stored, or that would leave the work tree refuses the whole rewind
before anything is written — the never-merge stance of
turn-restore-from-change-records, applied to the set. A confirmed rewind is
written down whole and synced first, then carried out: files, the step log
dropping the undone steps (a later rewind would otherwise see them as
conflicts), History, transcript, and the cut Turns' change sets, retracted as
an edit-last prune's are. Every part is idempotent, so the chat's next load
finishes a rewind a crash cut off. The chat stays idle afterwards.
Rejected: rewinding only to a user message (a long Turn could not be entered);
separate conversation-only and files-only rewinds (two states that disagree);
restoring what can be and skipping conflicts (a conversation that no longer
describes the files); continuing the run from the step automatically (the user
rewinds to change course, so the next word is theirs).

## step-checkpoints-are-bounded-per-chat

`b59bc27`. A chat's step checkpoints are bounded where they are used, not only
when the chat is deleted: at each Turn's admission, under the execution lock,
the oldest steps go past 1200 (down to 1000) and, while the store exceeds 1
GiB, the oldest fifth goes at a time. The store is then repacked: the blobs
the remaining steps reference are copied into a new store that replaces the
old one (a crash between the two renames is completed on the next run). A
rewind marks the store for the same repack, since the steps it undid leave
blobs nothing references. The newest dropped step's timestamp is kept, and a
rewind to a point before it is refused — it could not undo everything after
its point. Maintenance waits while a rewind is still to finish.
Rejected: growth until the chat is deleted (a long task's chat is exactly the
one that keeps running); libgit2 garbage collection (it has none); age-based
expiry (an idle chat would lose its checkpoints while nothing happens in it).

## agent-commands-run-under-git-bash-on-windows

`c9cdc69` and `648d4b2` (issue #52's cross-platform work). Agent commands —
the bash tool and every harness
exec the engine mounts — run under Git for Windows' bash when an install
exists, found in the one order core's harness discovery also uses:
`where bash.exe` (WSL's `System32\bash.exe` excepted), the install root
derived from a resolvable `git.exe`, then the standard install
locations. cmd.exe — the shell COMSPEC names — stays the floor, and
terminals keep it: only the agent toolset prefers bash. Rejected:
COMSPEC always (on a stock Git-for-Windows machine the registry PATH
carries `Git\cmd`, never `Git\bin`, so every sh-syntax command died with
cmd's "'printf' is not recognized"); WSL bash for agent commands (it
boots a Linux world win32 argv and cwd do not translate into — core's
harness supports it through a stdin transport the engine's argv model
cannot); flipping `login_shell()` itself (drags the terminal surface,
whose page owns that observable, into a toolset decision).

## addressed-path-vocabulary

`873900d`. Every path that crosses a boundary — tool arguments, RPC replies
(file listings, reads, renames, watch frames, the git status `workdir`),
persisted turn records (a subagent's `resultPath`, change sets), and the
verbs' error text — is the "addressed" spelling: `/` separators, an uppercase
Windows drive, `\\?\` unwrapped, UNC as `//server/share`, `.`/`..` collapsed
lexically and no symlink resolution. A native `Path` exists only inside the
operation that touches the disk. Rejected: keeping each platform's native
spelling and folding at every comparison (tab ↔ tree row ↔ watch frame ↔
grant is four independent string matches, and Windows's `\\?\` prefix defeats
`starts_with` silently — the whole class of bug this branch spent its time
on); persisting the verbatim spelling (records would stop matching a path the
user types or a checkout on another machine); matching grants
case-insensitively (the grant would then cover a differently-cased sibling the
user never approved, so a case-different path re-asks instead). One rule the
spelling keeps: a backslash outside a Windows-shaped root is a filename
character, never a separator. Superseding this re-opens the UI's string
matching and the persisted record spellings together — see
[the Windows survey](research/windows-compatibility.md) §2 and `tools.rs`
(`normalize_absolute`, `addressed_canonical`).

## ui-strings-live-in-a-compile-time-catalog

`feat/i18n` (survey: [docs/research/gpui-i18n.md](research/gpui-i18n.md)).
Interface strings leave the code for a holt-owned catalog —
`crates/ui/locales/<locale>/<group>.yml`, one file per migration group per
language, rust-i18n `_version: 2` YAML — embedded into the binary at compile
time by `rust_i18n::i18n!` at the crate root and read at render sites as
`t!("group.key")`. `en-US` is both the source language and the fallback, so
an unmigrated surface keeps rendering English and a missing key renders as
the key, never a blank or a crash. A key's first segment is the file it
lives in, so twelve groups migrate in parallel on disjoint key spaces with no
shared file. The language is a device-local `UiSettings` key
(`interfaceLanguage: Option<String>`): `None` follows the OS, resolved once
per launch through `sys-locale` and never written back — the `appearance`
policy — and an explicit choice is sticky. A switch is `settings::update` →
`set_locale` → `refresh_windows` → `set_menus`: the locale is a process
global outside gpui's change tracking, no `notify` reaches an imperative
`t!` read, and the native menu bar is a snapshot. In scope: every string the
UI composes for a person, OS notification bodies included. Out: text facing
the model (prompts, parser markers such as `COMMENT_BLOCK_HEADER`), logs,
engine and RPC content, user and workspace content, and identity strings
(actions, element ids, serialized theme ids, the built-in theme names).
Rejected: the Fluent stack (zero GPUI adopters; four-plus crates and a
translation platform for CLDR plurals a ~1,250-key catalog does not need);
gettext (C toolchain, source-string keying); a homegrown catalog (rebuilds
the macro embedding and BCP-47 fallback rust-i18n already ships, which
gpui-component runs in production); waiting for Zed upstream (#7409 unowned
since 2024-02); an engine-owned locale RPC (a device preference belongs in
`ui-settings.json` beside appearance and fonts — new engine surface for
nothing); one `ui.yml` (one file is one merge conflict for every parallel
migration); rust-i18n's v1 per-locale files (the locale comes from the file
*stem*, so `en-US/shell-chrome.yml` would register a locale named
`shell-chrome`); bare `en`/`zh` tags (sub-tag fallback would silently mask a
mistagged entry); and Zed's RFC stance that embedding is a non-starter —
right for a public translation pipeline, inverted for a fixed language set
shipped with the binary, where no runtime asset IO and no partial
translation on disk are the point.

## self-update-trust-is-per-platform

`#61` (engine stage; release-pipeline stage to follow). Windows and Linux
fail closed unless the release carries a `manifest.json` asset mapping
each artifact name to its sha256, and only macOS may install without one —
its DMG path verifies the Developer ID requirement instead, a chain this
decision stacks on and never weakens. Availability is reported from any
non-prerelease build; only applying is gated on a managed install (an
`.app` bundle, a release-marked Windows directory, an AppImage), so a
`cargo run` or hand-copied binary can see an update but cannot take it.
Rejected: trusting the download URL alone for the unsigned archives
(TLS protects the pipe, not the artifact, and there is no signature to
check); skipping digests everywhere to mirror macOS (would keep
Windows/Linux locked out of self-update entirely); and gating the check
loop itself on a managed install (the issue's acceptance asks unmanaged
installs for availability only).
