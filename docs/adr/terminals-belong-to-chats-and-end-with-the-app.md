# Terminals belong to chats and end with the app

Holt's built-in terminals belong to individual Chats, even when those Chats
share a Space, so switching conversations restores the user's corresponding
shells without mixing them with another Chat's terminals. Hiding or moving
the terminal panel preserves its sessions; closing a terminal ends its
process, and exiting Holt cleans up terminal processes instead of keeping
a separate background host for restoration across app restarts. Terminals
are operated by the user, independently of the agent's bash tool, which
keeps the existing tool execution and permission model.

Any close, Chat deletion, or app exit that would end running terminals
requires user confirmation; cancellation leaves the sessions running.
Shells that have already exited retain their output until explicitly closed
or restarted.
