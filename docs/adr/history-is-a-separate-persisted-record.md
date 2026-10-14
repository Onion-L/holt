# History is a separate persisted record from the Transcript

A chat's **History** (the model-facing `AgentMessage` sequence) is persisted
on its own, next to but independent of the **Transcript** (the rendered
rows). Before this decision only the Transcript was written to disk: a chat
reopened after a restart displayed its full history while the model started
from an empty context. The History lives in a holt-owned JSONL file — a
version header line, then one entry per line — where the only entry kinds
are `message` (an `AgentMessage` in its upstream serde shape, unchanged) and
`compaction` (summary text, cut point, tokens before/after, trigger). It is
appended to as each message completes during a Turn, not rewritten at Turn
end, and replayed linearly on load.

## Considered options

- **Rebuild the History from the Transcript on load**: rejected. Transcript
  entries are sanitized display structures (tool calls folded into chips,
  content trimmed); reconstructing model messages from them is lossy, and a
  model "remembering" something different from what the user sees is worse
  than a model that visibly remembers nothing. Chats created before this
  record existed therefore open with an empty History and a persisted
  Transcript notice saying earlier conversation is not visible to the model.
- **Reuse pi-core-rs's session JSONL (v4) codec**: rejected. It is a
  tree-shaped session model (parent ids, branches) that holt does not use,
  and it would bind holt's on-disk format to upstream evolution. Only the
  `AgentMessage` serde is reused — upstream pins it with golden tests.
- **Write the History once per Turn** (as the Transcript is written):
  rejected. A Turn can run for minutes across dozens of tool calls; a crash
  mid-Turn would lose every completed tool result from the model's memory
  while the Transcript still shows them — the same visible-but-forgotten
  defect, one level down.

## Consequences

- **Repair before write, and on load.** The History must always be a valid
  request payload: no `tool_use` without a `tool_result`, no trailing
  half-streamed assistant message. An interrupted Turn keeps its partial
  assistant message with the stop reason rewritten to a normal end and a
  synthetic error `tool_result` ("interrupted by user") per dangling call,
  so the next Turn can see what was attempted. A Turn that errored keeps the
  user prompt and drops the erroring assistant message (any partial content
  in it is handled as above). The same repair runs on load to absorb a
  crash-truncated tail. pi-core-rs's `transform_messages` performs a
  similar repair at request time, but it discards aborted assistant
  messages outright — which is why holt repairs on write instead of relying
  on it.
- **Corrupt or unreadable History files do not block the chat.** The chat
  opens with an empty History and the same Transcript notice (with the
  reason); the file is renamed aside, never overwritten.
- **Cross-provider switches are not holt's concern.** Foreign thinking
  signatures, tool-call id formats, and unsupported images are normalized by
  pi-core-rs at the provider layer; holt does nothing extra.
- Deleting a chat deletes its History file with its Transcript.
