# Metadata-only skill advertising with read-tool progressive disclosure

The agent learns which skills exist from a metadata-only
`<available_skills>` block — name, description, location, never content —
appended to the per-run system prompt through pi-core-rs's
`format_skills_for_system_prompt`, honoring `disable-model-invocation`.
Full content reaches the model by progressive disclosure only: the model
serves itself by calling the existing `read` tool on the advertised
location, or the user forces it with the `/skill` slash command, whose
engine side queues an ordinary run whose prompt is
`format_skill_invocation` output (the `<skill>` block plus any extra
instructions; the raw directive text is never sent). Alternatives surveyed
across pi, kimi-code, and codex (2026-09): kimi's dedicated `Skill` tool
with host-side injection was rejected because it adds a tool and a mid-run
steer pipeline that `queue_command` currently rejects outright; codex's
per-turn developer-message placement was rejected because `AgentContext`
has no developer-message channel and holt rebuilds context every run
anyway, which is precisely the freshness codex's placement buys. Codex's
catalog budget cap (truncate or omit descriptions past a limit) is adopted
as a guard against pathological catalogs.

## Consequences

- Zero new agent tools and zero new injection plumbing: the loader and
  both formatters are upstream pi-core-rs functions holt already links,
  and holt follows the upstream host contract rather than inventing one.
- The block is only meaningful while the read tool is mounted (pi guards
  the same way and skips it otherwise); holt mounts read unconditionally.
- Models do not reliably read the skill file on their own — pi's own docs
  admit this. The manual `/skill` trigger is the forcing function, and
  skills that must never be model-invoked carry
  `disable-model-invocation`.
- The transcript renders `/skill` invocations and reads of `SKILL.md` as
  compact chips; full text goes to the model context, not the transcript.
- No host-side argument substitution (kimi's `$ARGUMENTS` expansion):
  extra instructions are appended verbatim. It can be layered into the
  invocation path later without touching the advertisement.

Amendment (ADR-0035): the manual trigger is no longer the `/skill` slash
command — the user forces a skill with an inline `$` mention in an ordinary
message (bare `$name`, or the linked `[$name](SKILL.md path)` the `/` menu
inserts), resolved path-first at admission. The transcript chip for what
the model was told moved to the agent entry; reads of `SKILL.md` still
collapse to chips. Everything above about advertising and disclosure
stands.
