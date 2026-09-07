# Subagents use independent History and return foreground results

Subagents start with a Task brief and applicable project instructions and
skills metadata, without copying the parent's History. Each foreground
delegation returns the Subagent's final summary as its tool result; several
delegations may execute in parallel while the parent waits. This keeps
intermediate investigation out of the parent's History, at the cost of
requiring the parent to supply sufficient background and acceptance criteria.
Background execution, completion notifications, and follow-up instructions
are outside this implementation's scope. These decisions were confirmed on
2026-09-07; see the [design](../../.scratch/subagents/spec.md).

The parent Turn owns child execution: Stop or Steer cancels all children and
waits for cleanup before the next Turn. Child records survive restart for
inspection, but execution does not resume. Workers share the parent's working
directory and Permission mode, with file ownership assigned by the parent;
there is no automatic worktree isolation, conflict merging, or rollback.
This keeps delegation within the existing Turn lifecycle and working-copy
model, at the cost of relying on agent coordination for concurrent edits.
