# Skills referenced in place from standard roots

Holt's skill catalog is assembled at load time by scanning three standard
roots — project `.agents/skills` (at the chat's cwd), personal
`~/.agents/skills`, and holt's own `~/.holt/skills` — through pi-core-rs's
`load_skills` (multi-root traversal, missing roots skipped silently, ignore
files honored, invalid entries returned as diagnostics). Holt never copies,
moves, or registers skills: a skill becomes available by being placed in a
root, and name collisions resolve nearest-root-wins (project > personal >
holt). Copy-on-import into the data dir was rejected because it goes stale
against its source and forfeits the interop point of the shared
`~/.agents/skills` layout; an explicit registration surface (`/skill add
<path>` plus a persisted index) was rejected for v1 because a
convention-based catalog needs no persistence, no new mutations, and no
lifecycle UI — it is the same cause as ADR-0002's no-repo-registry stance:
the filesystem is the registry.

## Consequences

- Skills stay fresh with their source and interoperate with the Agents
  Skills ecosystem: other agents' skills appear for free, and holt-visible
  project skills are visible to other agents that honor the same layout.
- A skill vanishes when its source does. The catalog must therefore expose
  load diagnostics and shadowed entries through a Skills page in Settings;
  the composer's `/` skill menu lists only valid, unshadowed skills.
- No per-skill enable/disable, arbitrary-path registration, or network
  sources yet — each would require the persisted index this ADR defers.
- The catalog is rescanned when runs assemble their context; there is no
  invalidation problem, and no cache.
