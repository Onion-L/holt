# Code standards

Write and review changes against these. The list starts small and grows by
two mechanisms only: a lesson in [RETROS.md](RETROS.md) seen on three
separate days is promoted here as a standing rule, or the maintainer adds
one directly. The three seed rules
below predate that mechanism — carried over from the pre-restructure
AGENTS.md (c526a91^); every other rule arrives by promotion, with its
RETROS evidence, or names its directive.

- **Dependencies**: check `Cargo.toml` and existing imports before adding
  one; prefer existing helpers and types over parallel abstractions.
- **Async and locks**: keep cancellation and lock boundaries consistent
  with neighboring code.
- **Surfaces**: do not change public types, RPC methods, or serialized
  representations without checking every consumer and the owning subsystem
  page.
- **Vendored gpui**: modify `third_party/gpui` only with a complete,
  indisputable justification (maintainer directive).
