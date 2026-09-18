# Model setup runs in a scoped setup chat; the agent never applies

The v1 surface (ADR-0029: a proposal tool plus an apply tool behind a forced
approval, mounted in every chat) is replaced by a fixed flow: a hidden
`model-setup` chat (found/created by `EnsureModelSetupChat`, scoped via
`ChatConfig.scope`) whose toolset is exactly web research plus the read-only
`model_proposal` — no file tools, no delegation, no apply tool — under a
fixed four-step workflow prompt. Normal chats mount neither tool: their
catalog-write capability is nil, so prompt-level "don't misuse it"
constraints are gone rather than relied on. The only write path is the
Settings review panel's `ApplyModelProposal` RPC, which re-validates and
transactionally (baseline CAS) applies a stored proposal; the button is the
human approval. ADR-0029's proposal/apply separation and its gate invariant
(a `model_apply` call, should one ever be mounted again, still forces human
approval) carry over unchanged.
