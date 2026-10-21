# Crash-injection eval

Kills `holt-runner` with `kill -9` partway through a task, restarts it, and
checks the task's end state. It measures whether a crashed task still gets
done and what the crash costs, for three arms:

| Arm | After the kill |
| --- | --- |
| `baseline` | no kill |
| `resume` | `holt-runner --resume` on the same data dir: Holt's own crash recovery (ADR `turns-resume-from-an-intent-journal`) |
| `cold` | orphans reaped, then a fresh run of the same prompt in a new data dir over the same work tree: an agent that remembers nothing |

The design and the survey behind it are in `docs/research/long-horizon-eval.md`.

## Run

```bash
cargo build -p holt-runner
scripts/crash-eval/run-suite.sh --tasks scripts/crash-eval/tasks \
  --out /tmp/holt-crash-eval/run1 --model zai-coding-cn/glm-5.3 \
  --repeats 3 --jobs 3 --kill-after 15:40
scripts/crash-eval/summarize.py /tmp/holt-crash-eval/run1
```

`--kill-after S1:S2` draws each kill time uniformly from that range (seconds
after the run starts); `--kills N` kills that many times per task (the engine
stops resuming after 3 cut-off restarts, so keep it at 2 or below). Provider
credentials come from `~/.holt` (or `HOLT_DATA_DIR`).

`--skip-existing` skips any case whose `result.json` is already on disk, so a
suite cut off overnight continues where it stopped. `HOLT_RUNNER` overrides
the binary under test: point it at a copy outside `target/` (or run the suite
from a pinned `git worktree`) when a rebuild must not change the runtime
mid-suite.

## Tasks

`tasks/` holds local tasks that need nothing but bash and Python, built so a
kill lands inside a long command:

- `progress-loop`: a 60-second loop that appends lines; repeating it duplicates them.
- `inbox-migrate`: a batch job that books records and then moves them; a blind rerun double-books.
- `slow-tests`: fix bugs, then a one-minute test suite has to pass.

A task dir holds `instruction.md` and `check.sh` (exit 0 = pass, run in the work
tree), and optionally `setup.sh`, `on-kill.sh`, `teardown.sh`, and `timeout`
(seconds). Hooks see `TASK`, `WORK`, and `OUT`. The work tree is made a Git repo
if setup leaves it without one, so step checkpoints are recorded.

### Terminal-Bench 2

`tb2-task.sh` wraps a Terminal-Bench 2 task in the same format. The task's
container runs with its working directory bind-mounted from the work tree. The
generated task dir carries a `shell.sh`, which `run-task.sh` passes to
`holt-runner --shell`: every bash call the agent issues is forwarded into the
container, so the model writes `ls` rather than a `docker exec ... bash -lc`
prefix. After a kill, `on-kill.sh` kills what the dead run left running inside
the container, which Holt's launch-time reaping can't reach. Needs Docker
running.

Use the 2.1 task set — 2.0 has 28 tasks that reject valid work (external
dependencies, budgets, instructions that did not match their tests):

```bash
git clone --depth 1 https://github.com/harbor-framework/terminal-bench-2-1 /tmp/tb2
for t in /tmp/tb2/tasks/*/; do
  s=$(awk '/^\[agent\]/{f=1;next} /^\[/{f=0} f&&/timeout_sec/{print int($3)}' "$t/task.toml")
  [ "${s:-0}" -ge 1800 ] && scripts/crash-eval/tb2-task.sh "$t" "/tmp/tb2-long/$(basename "$t")"
done
scripts/crash-eval/run-suite.sh --tasks /tmp/tb2-long --out /tmp/holt-crash-eval/tb2 \
  --model zai-coding-cn/glm-5.3 --arms "baseline resume cold" --jobs 4 --kill-after 60:120
```

The generated task dir records the image in `image` and the upstream task in
`source`, and `check.sh` reads the upstream tests by absolute path — generate
the set on the machine that runs it, or keep the clone at the same path.

## What each result records

`OUT/<task>/<arm>/<repeat>/result.json`: `pass`, `wallSecs` (from first start
to end, across restarts), `kills`, `totalTokens` / `modelRequests` /
`toolCalls` counted from the History of every data dir the task used, and
`repeatedCommands` (bash commands issued again verbatim). Tokens come from the
History, not the usage ledger: the ledger books a Turn only when it settles,
so a killed run's calls are in the History alone. A model call still streaming
at the kill left nothing behind and is not counted, so crash arms' token costs
are a lower bound.
