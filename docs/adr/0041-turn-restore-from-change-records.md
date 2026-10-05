# Turn restore writes back from the change record

## Context

ADR-0024 persists an immutable before/after snapshot per settled Turn and
leaves undo as future work.

## Decision

`RestoreTurnChanges` restores a settled Turn's files by plain file I/O from
that record; Git is used only to locate the work tree root. A file is written
only while its current content hash equals the record's `newContentHash`;
otherwise it is refused (`conflict`, or `laterTurn` when a later settled Turn
touched the path). Added files are deleted, deleted files recreated, renames
moved back. Truncated, binary, non-UTF-8 (stored text no longer hashes to
`oldContentHash`), symlinked, and `.git` paths are refused, as is any chat
with a running Turn. Outcomes are per file (partial restore is allowed);
`dryRun` reports them without writing. Records are never modified, so a repeat
call reports `alreadyRestored`.

## Consequences

Permission bits, symlinks, and concurrent writers between the hash check and
the write are not handled. Non-Git spaces capture no records and are out of
scope.
