# Agents read Chats through validated Chat links

Holt agents read another Chat through a dedicated `read_chat` tool whose input
is a complete Chat link. The tool validates the link's opaque Workspace
locator and requires the target to exist in the current Workspace registry;
archived Chats remain readable, while foreign, deleted, unsafe, corrupt, and
current-Chat targets fail explicitly. It returns Chat metadata and only the
user/assistant text visible in the Transcript, never the model-facing History,
reasoning, tool details, or internal notices. Chat content is untrusted data,
not instructions.

The result defaults to the latest 20 visible messages, paginates backward with
an absolute `before` position, and is capped at 32 KiB per call with explicit
truncation. A single message over that budget returns only the final content
that fits within the result budget with `message_truncated` set; the tool does
not add character-level paging.
Running target Chats are read from their current in-memory Transcript snapshot;
unloaded Chats are read directly from storage without creating a runtime or
repairing persistent state. Parent agents, Explorers, and Workers all receive
the read-only tool. Its Transcript chip shows the target Chat title, falling
back to its id, so cross-Chat access remains visible to the user, but does not
navigate when clicked. Tool results use readable Markdown plus structured
pagination details and persist in the calling Chat's History and Transcript
like other tool results.

This keeps Chat-link handling inside a bounded, Workspace-aware capability
instead of teaching models to inspect Holt's private files. Reading the
Transcript preserves what the user actually saw; reading History would expose
internal tool traffic and would produce different results after Compaction.
The Chat-link parser and Workspace-locator calculation belong to `holt-proto`
so the UI and engine share one protocol. The system prompt directs agents to
call `read_chat` immediately for a user-supplied Chat link instead of searching
the working directory or Holt's private storage.
