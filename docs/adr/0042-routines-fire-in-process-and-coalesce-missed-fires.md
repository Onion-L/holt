# Routines fire in-process and coalesce missed fires

## Context

Routines run a prompt on a cron schedule, unattended. Holt has no daemon: the
engine lives inside the app process. A scheduler that must fire while the
user is away has to decide what happens when the process is not running, and
what happens when an unattended run blocks on a human.

## Decisions

- **The scheduler is an engine task, alive exactly as long as the process.**
  Routines fire with or without a window, never while Holt is quit. No
  launchd agent or helper daemon; whether Holt should grow one is an open
  question, not part of this decision.
- **Missed fires coalesce into one catch-up run.** Each Routine persists its
  `last_fired_at`; at startup (and after the device wakes) every fire between
  it and now — however many, however old — becomes a single run marked as a
  catch-up with the number of fires it stands in for. Missed fires never run
  one-for-one.
- **One live run per Routine.** A fire (scheduled, catch-up, or Run now) that
  arrives while the previous run is still running or waiting is recorded as
  skipped, with no Chat.
- **A blocked run waits for the human; there is no gate timeout.** A run that
  hits an Approval or a question is marked waiting and posts a system
  notification even when a window is active (unless that run's Chat is in
  view). It stays waiting until answered or interrupted, and later fires are
  skipped meanwhile. New Routines default to Auto review to make this rare.
- **The Run outcome is the first Turn's outcome.** Follow-up Turns in the
  run's Chat do not rewrite it.

## Consequences

A machine that is off or has Holt quit runs nothing; the catch-up run is the
only trace. A forgotten waiting run silently suppresses every later fire of
its Routine until someone answers it — the notification and the Scheduled
nav indicator are the only safeguards.
