# Automatic titles run outside pi-core hooks

Holt generates a chat's automatic title in a one-shot engine-owned background task started when the first user prompt is accepted, rather than through a `pi-core-rs` hook or middleware. The current low-level loop exposes request and tool callbacks, but the harness hook registry is unimplemented and no title generator exists; an external task can run in parallel, use a separately configured provider/model, and fail without affecting the Turn.

Title settings are engine-owned and exposed through typed RPC. A title task receives only the first user prompt, writes no History or Transcript entries, and may update the chat only while its title source remains automatic and the task's first-prompt generation is still current. Manual renames lock the title; legacy non-empty titles are treated as manual. An empty model disables the task, and failures retain the synchronous fallback without retries.
