# API keys are collected through key requests, never as chat text

**Status:** amended by ADR-0037. The card moved from the Settings dialog
into the Provider Mode transcript, destination approvals persist with the
chat instead of the session, and a draft provider can be addressed. The
key rules below are unchanged.

The setup chat can raise a **key request**: a `request_provider_key`
tool (mounted only there; normal chats never see it) makes the engine
hold a pending request that the Settings dialog renders as a card above
the composer — provider, resolved destination, masked input. The value
goes straight to the credential store via one engine-owned settle RPC
that atomically saves the key, records the destination approval, queues
a fixed notice ("API key saved for X — re-probe"), and clears the
request; dismissal queues the counterpart notice. The key never enters
History, a tool argument, or the Transcript — so a fetched page cannot
read it back. This deliberately relaxes ADR-0029's "apply before a probe
may carry a key": entering a key now itself approves the destination the
card showed, bound to the exact (chat, provider, baseUrl) triple — a
changed baseUrl re-arms the request — and the planned-probe SSRF
public-host gate stays. No turn pauses: the requesting turn ends, and
the notice starts the next one through the ordinary queue. Rejected:
keeping the key behind the provider panels only (the user had to close
the dialog, which deletes the session-scoped setup chat and its
proposals), and gate-style mid-turn pausing (new engine machinery to
save one round-trip). The Manual tab's optional key field writes through
the same store; an empty field leaves any stored key untouched.
