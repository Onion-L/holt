# ask_user: a question card, not a blocking tool

The agent sometimes needs the user to pick between enumerable choices
before it can proceed. Claude Code ships this as a blocking
`AskUserQuestion` tool whose result is the user's selection; holt already
has the machinery that shape needs — pending approvals that pause the Turn
behind an RPC — but none of it fits: the permission vocabulary
(allow / always-allow / deny, grants, auto-review) answers "may I?", while
a question's answer is arbitrary content, and a blocked tool call held in
memory goes stale across restarts.

Instead the question card reuses the pattern Provider Mode proved with
`choose_provider` (ADR-0037): a **non-blocking tool that lands a card and
stops the Turn**, with the answers arriving as an ordinary user message.

- The `ask_user` tool takes 1–4 questions, each with 2–6 distinct
  options, most likely first — one card answers several related
  questions the way Claude Code's `AskUserQuestion` pages 1/N, without
  splitting them across turns. Its execute never waits: it validates and
  returns, and the run loop's card landing (the `tool_card` dispatch)
  appends a `MessagePart::QuestionCard` after the tool part. The tool
  description — not the system prompt — carries the calling contract:
  don't also ask in text, STOP the Turn after calling, open-ended
  questions stay in text. The system prompt only routes: enumerable
  choices go to `ask_user`.
- While pending, the questions render in the composer's approval bar —
  its third kind, beside the gate and the plan (ADR-0014/0025) — one
  page per question: that question is the title, its enumerated options
  are keyboard-first rows (arrows, digits, Enter), the note row is the
  free-text answer, and a `1/N` pager shows the position. A blank note
  is not an answer and keeps the page up. Answering a page stashes its
  answer and opens the next question; the last page's answer submits
  everything in one `SettleQuestion` (`{chatId, cardId, choices}`),
  which stamps the card Chosen under one transcript lock (a second
  concurrent answer is refused), queues each question restated with its
  answer as an ordinary user message through the same enqueue the
  provider choice uses, and rolls the stamp back if the enqueue fails.
  The transcript builds no row for a pending card (the gate's rule —
  the bar is the only interactive surface); the settled card lands as a
  small marker row, one line per answered question.
- Escape dismisses the card unanswered. A question blocks nothing — the
  Turn already stopped — so unlike a gate (whose Escape interrupts), the
  bar just closes and `DismissQuestion` (`{chatId, cardId}`) stamps the
  card Superseded, persistent like every card state: a restart does not
  resurrect a dismissed question. The model reads the unanswered card
  next Turn.
- Any new Turn retires still-pending question cards (`Superseded`) — a
  typed answer moved the conversation past them; the click that queued the
  Turn itself already stamped its card Chosen. Card states ride the doc,
  so the lifecycle is restart-safe and multi-device by construction, the
  same property every transcript card already has.

Deliberately not built: multi-select within a question, per-option
sub-questions, and a same-Turn tool result. The first two are
speculative; the third would require turn-blocking machinery for one
round-trip of latency. The answers-as-user-message shape costs one extra
Turn and matches how plan approval and provider choice already continue
conversations.

Plan Mode and Provider Mode mount no `ask_user` (their whitelists keep
their own asking conventions — plain text in planning,
`choose_provider` in Provider Mode), and neither do subagents: a
child's questions are its findings, reported through the parent.
