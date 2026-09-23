# Skill invocation is an inline `$` mention

Explicit skill invocation is not a slash directive. A user forces a skill by
writing a mention inline in an ordinary message — the bare `$name` token or
the linked form `[$name](SKILL.md path)` — anywhere in the text; the
composer's `/` menu inserts the linked form as an atomic `$name` chip. At
Turn admission the engine parses the prompt's mentions and resolves each
path-first: the linked form matches the exact file it points at (winners and
shadowed catalog entries alike, `~`/relative paths expanded against the
chat's working directory) and falls back to a name lookup against the
invocable winners; the bare form resolves by name alone. Every resolved
mention's `<skill>` block (pi-core-rs `format_skill_invocation`, unchanged)
is prepended to the model-visible prompt and seeded as the opening chip of
the run's transcript entry; unresolved mentions stay ordinary text — no
error, no env-var blacklist, because a `$` that resolves to nothing is just
the user's `$HOME` or `$100`.

This amends ADR-0006's manual trigger (the `/skill` slash command) and keeps
everything else it decided: metadata-only advertising, progressive
disclosure through the read tool, `disable-model-invocation`, no host-side
argument substitution, and the catalog budget. ADR-0005's nearest-root
shadowing is untouched as catalog policy — but a linked mention bypasses it,
because the path is authoritative: the link points at a concrete file, and
invoking that file is what the user asked for. The dedicated
`SessionCommandPayload::InvokeSkill` command and the `PendingKind::Skill`
queue kind are retired; the variant survives deserializable so old command
ledgers decode, and `Queue::load` migrates persisted Skill pending items
into mention text (`$name` plus the extra instructions) so a restart never
drops them.

Alternatives: keeping the exclusive `/skill` queue item alongside mentions
was rejected — one invocation path means one parser, one error story, and
one queue semantics (a mention is part of a message, so staging, editing,
and last-message edit apply uniformly). Name-only invocation without the
linked form was rejected: the path is what disambiguates same-name skills
and makes the composer chip a stable pointer rather than a string that
resolves differently tomorrow. Hand-rolled host-side injection of a
dedicated skill tool (kimi's shape) remains rejected for the ADR-0006
reasons. The syntax and inline semantics follow Codex's `$` tool mentions;
the composer chip mechanics follow ZCode's canonical-markdown-under-a-chip
projection, which holt's raw-text composer projection already mirrored for
file mentions.

## Consequences

- A message can invoke several skills and carry instructions around them —
  the queue no longer has a skill-specific item, and "extra instructions"
  collapse into the message body itself.
- Submit-time rejection narrows to truly broken requests (a mention naming a
  skill disabled in Settings → Skills); a deleted or misspelled skill turns
  into plain text at admission instead of a retained queue error — the
  prompt the model sees always matches what the user can read.
- Transcript user bubbles render the raw mention inline as a chip (accent,
  click opens the `SKILL.md` through the shell file-open event); legacy
  transcripts with `MessagePart::Skill` user chips keep rendering — no
  migration.
- `$` in ordinary prompts is safe by resolution, not by blacklist: the
  engine only scans when a `$` is present, and an unresolvable token costs
  one failed catalog lookup, never a rewritten prompt.
