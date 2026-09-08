# File editing state belongs to Spaces

File tabs, unsaved contents, and file-tree expansion state belong to a
Space and are shared by its Chats. Independent per-Chat drafts would let
two Chats edit the same working-directory file without seeing each other's
unsaved changes. Switching Chats within a Space therefore keeps the same
file editing state, while switching Spaces selects that Space's state.

Files use the contents tab strip alongside Chat-owned Terminal, Diff, and
Subagent views; sharing the presentation does not transfer ownership of
those views to the Space. This deliberately differs from the Terminal
ownership decision in ADR-0017. Navigation state is restored across app
restarts; unsaved contents are retained during navigation but require a
Save, Discard, or Cancel decision when they would be closed. The first
release does not persist unsaved drafts for crash recovery.
