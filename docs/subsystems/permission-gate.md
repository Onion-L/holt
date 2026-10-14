# Permission gate

How a chat's mutating tool calls are gated: the permission mode picks the gatekeeper — a human Approval, one model review pass, or none — with always-allow session grants on top and a sticky device default for new chats. The whole gate rides the agent loop's before-tool-call hook; the agent core knows nothing about it.

Not here: Plan Mode and Provider Mode (see modes) reshape the toolset before the gate ever sees a call; the MCP pool that mounts `mcp__` tools (see toolset) — though the rule that every `mcp__` call is presumed mutating lives here. What an Approval does to the queue's pacing belongs to conversation-runtime.

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Gate predicate | Tool identity only — `write`, `edit`, `bash`, and every `mcp__`-prefixed call (foreign code carries unknown risk; a server's hints about itself are never trusted). No command parsing, no path confinement | [`crates/engine/src/gate.rs`](../../crates/engine/src/gate.rs) (`is_mutating_tool`) |
| Forced approvals | `model_apply` always meets the human gatekeeper, whatever the mode: a catalog write steers where the API key is sent, and full-access means "don't audit my files", not "don't audit my key's destination". Grants never exempt it; auto-review never substitutes | `gate.rs` (`forces_approval`) |
| The hook | One gate hook per run; the mode is the Turn's snapshot — a mid-Turn switch leaves the running Turn under its original mode. The verdict wait races the run's cancellation token, so no Approval can outlive its Turn | `gate.rs` (`before_tool_call_hook`) |
| Session grants | In-memory, chat-scoped, never persisted — cleared on restart. Bash grants match by word prefix on the command (extension by plain arguments only); write/edit by exact resolved path; MCP by the exact two-level `mcp__server__tool` name. Checked before the gatekeeper, so they hold across mode switches | `gate.rs` (`GateGrants`) |
| Auto-review | One completion on the chat's own model and transport — no separately-configured reviewer — no tools, a small token cap, silent provider retries. The reply protocol is strict: only a bare `APPROVE` or `APPROVE: reason` passes; a hedged or garbled answer rejects | `gate.rs` (`run_review_pass`, `parse_review_reply`) |
| Approval lifecycle | A pending gate chip stamped into the call's Tool part; `ResolveApproval` answers allow / always-allow / deny (with a note addressed to the model). Interrupt settles the wait as aborted; still-pending gates settle as aborted on transcript load | `gate.rs`, [`rpc/modes.rs`](../../crates/engine/src/rpc/modes.rs) |
| Mode tiers | `confirm-changes` (the default), `auto-review`, `full-access`; sandbox-era stored values remap on read | [`crates/proto/src/agent.rs`](../../crates/proto/src/agent.rs) |
| Sticky default | `permission-mode-default.json`: new chats inherit the device's last choice; a missing, empty, or corrupt file loads the default and is left untouched — a preference record must never brick boot | [`crates/engine/src/mode_default.rs`](../../crates/engine/src/mode_default.rs) |
| UI cutaways | The gate chip on the Tool part; the composer's approval bar (Approvals, plan cards, and question cards are its three kinds) | [`crates/ui/src/transcript/approval.rs`](../../crates/ui/src/transcript/approval.rs), [`crates/ui/src/composer/approval_bar.rs`](../../crates/ui/src/composer/approval_bar.rs) |

## Runtime path

1. A tool call reaches the hook before the tool runs. Neither mutating nor forced → straight through (reads, `grep`, the web tools, delegation).
2. **Grant check** (skipped for forced tools and full-access): a passing grant stamps the chip `Exempted` and executes without asking.
3. **auto-review**: one model pass; a pass executes (chip `ReviewPassed`), a rejection blocks the call with the reviewer's reason (chip `ReviewRejected`), a cancelled pass leaves no verdict. Every pass bills to the chat's ledger under `auto-review`, whatever the verdict.
4. **confirm-changes**: a pending Approval is stamped into the Tool part and the Turn pauses behind it (a routine run's record shows waiting) until `ResolveApproval` answers or the Turn ends. Allow executes; always-allow also records the session grant; a denial settles as an **error tool result the model reads** — the note verbatim when one was written — and the Turn continues. Interrupt settles the gate as aborted.
5. **full-access**: mutating calls run ungated — except forced tools, which ask anyway.

## Constraints, state, and failure behavior

- **Fail closed, everywhere.** A garbled review reply rejects ("did not give a clear verdict"); a grant fails closed whenever the command line is anything but one plain command (control operators, redirections, substitution, unterminated quotes — no prefix match); a corrupt mode-default file loads the default.
- **Denials are content, not crashes.** The model reads the denial (with the user's note) and can change course in the same Turn.
- **Grants are the narrowest thing that works.** No server-wide MCP grants (a sibling tool of the same server still asks); bash grants extend by plain arguments only; restart forgets everything.
- **The mode is snapshotted at Turn start** — a switch lands from the next Turn, never retroactively on a running one.
- Reads, content search, web tools, and delegation are never gated; a subagent inherits the parent Turn's mode and its Approvals project into the parent transcript with a typed origin.

## Verification

```sh
cargo test -p holt-engine gate
cargo test -p holt-proto permission
```

The suites cover the predicate and forced tools, grant matching (prefix, path, exact MCP name; the fail-closed shell-word splitting), the strict review parser (hedged/garbled shapes), verdict settling (allow / always-allow / deny-with-note / abort), load-time settling of orphaned gates, and the tier remap.
