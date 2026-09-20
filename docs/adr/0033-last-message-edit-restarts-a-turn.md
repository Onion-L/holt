# Editing the last user message restarts the conversation from that turn

The UI may edit only the latest user message in a Chat's Transcript. The
editor is local while an active Turn continues; submitting the edit cancels
the active Turn, waits for its cleanup, removes that message's later
Transcript and History, replaces the user text while preserving attachments,
path references, and skill identity, then starts a new Turn with the chat's
current settings. Existing filesystem or external side effects and usage from
the cancelled Turn are retained: editing changes conversation state, not the
working tree or accounting history. The queue keeps its remaining order, and
editing is rejected if the message is no longer the latest user message when
submitted.
