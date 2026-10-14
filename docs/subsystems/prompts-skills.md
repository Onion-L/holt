# Prompts and skills

What the model is told before it acts: the system prompt (a universal template plus every `AGENTS.md` on the walk from the filesystem root down to the chat's cwd) and the skills catalog — three standard roots scanned fresh on every use, advertised metadata-only, and invoked as inline mentions whose linked form is path-authoritative.

Not here: the tools a Turn mounts (see toolset-mechanics — skills change what the model *knows*, not what it can *do*); the mode-specific prompt blocks (see modes); compaction, which rewrites the History, not the prompt (see conversation-policy).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| System prompt | A universal template (`system_prompt.md`) instantiated with the chat's cwd, plus workspace instructions: every `AGENTS.md` found walking from the filesystem root down to cwd is appended, under a character budget, so workspace knowledge lives in `AGENTS.md` files and never in the universal template | [`crates/engine/src/agent.rs`](../../crates/engine/src/agent.rs) (`system_prompt`, `AGENTS_INSTRUCTIONS_BUDGET`), [`system_prompt.md`](../../crates/engine/src/system_prompt.md) |
| Skill roots and catalog | Skills are referenced in place — nothing is copied, moved, or persisted; the filesystem is the registry and every use rescans, so there is no cache and no invalidation problem. Three standard roots (project `.agents/skills` at the cwd, personal `~/.agents/skills`, Holt's own under the data dir) with root precedence; shadowed entries stay reachable by path | [`crates/engine/src/skills.rs`](../../crates/engine/src/skills.rs) |
| Advertising | A metadata-only `<available_skills>` block appended to the system prompt when the catalog is non-empty, under an 8 KiB budget: descriptions truncate, then drop, then trailing skills drop — never an error, the task stays the bulk of the window. The model self-serves a skill's `SKILL.md` through the mounted read tool; those reads collapse into chips in the transcript | `skills.rs` (`SKILL_LISTING_BUDGET`), `agent.rs` |
| Inline mentions | A mention resolves at Turn admission against a fresh catalog: the linked form's path is authoritative (an exact file match against winners *and* shadowed entries, `~` and relative paths expanding against the cwd), falling back to name lookup against the winners; the bare `$name` form resolves by name alone. Resolved mentions prepend their upstream `<skill>` block ahead of the verbatim prompt text; unresolved ones stay ordinary text; a prompt with no `$` never pays for a scan | `skills.rs` (`resolve_mention`, `resolve_prompt_mentions`) |
| Legacy invocations | The retired `invokeSkill` queue command survives only as a deserializable legacy shape, migrated to mention text on load | [`crates/engine/src/queue.rs`](../../crates/engine/src/queue.rs) |
| UI cutaways | The composer's `/` menu inserts a skill as the linked mention; the transcript renders invocation and `SKILL.md`-read chips; Settings lists the catalog with shadowed and invalid entries | [`crates/ui/src/composer/slash.rs`](../../crates/ui/src/composer/slash.rs), [`crates/ui/src/settings/skills.rs`](../../crates/ui/src/settings/skills.rs) |

## Runtime path

1. **Turn admission** builds the system prompt: template + cwd, then the `AGENTS.md` walk under its budget, then the skills block when the catalog is non-empty.
2. **The catalog** is scanned fresh: the three roots in precedence order; winners are invocable, losers are listed as shadowed with their load diagnostics.
3. **The user's prompt** is rewritten for the model: each resolved mention's `<skill>` block is prepended (blocks joined, the text kept verbatim after), and the chips ride the transcript entry so the transcript shows exactly what was injected.
4. **The model self-serves**: it reads a `SKILL.md` through the ordinary read tool when it wants the full instructions; the decode path collapses that read into a chip.

## Constraints, state, and failure behavior

- **No skill state exists.** No copy, no install record, no cache — deleting a skill from its root removes it from the next scan; a skill present in two roots resolves by precedence unless the mention carries a path, which bypasses shadowing entirely.
- **Budgets degrade, never fail.** An oversized description truncates before it drops; an oversized catalog drops trailing skills; a pathological `AGENTS.md` cannot crowd out the task. None of it errors the Turn.
- **A mention resolves when the queue admits the item**, against that moment's catalog — never frozen at submission.
- **No host-side substitution**: the model sees the upstream `<skill>` block verbatim; the mention's arguments are ordinary prompt text.

## Verification

```sh
cargo test -p holt-engine skills
cargo test -p holt-engine agent
```

The suites cover the three-root scan and precedence/shadowing, path-first name-fallback mention resolution (including shadowed-by-path reaches), prompt rewriting (blocks prepended, text verbatim, `$`-free prompts skip the scan), budget truncation, and the `SKILL.md`-read chip collapsing.
