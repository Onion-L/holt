#!/usr/bin/env bash
# One task, one arm, of the crash-injection eval: run holt-runner against the
# task, kill -9 it mid-Turn, restart, and check the task's end state.
#
#   run-task.sh --task DIR --arm baseline|resume|cold --out DIR --model P/M
#               [--kill-after S | S1:S2] [--kills N] [--timeout S]
#
# Arms:
#   baseline  no kill.
#   resume    kill, then `holt-runner --resume` on the same data dir — Holt's
#             own crash recovery.
#   cold      kill, reap the orphans, then a fresh run of the same prompt in a
#             new data dir over the same (now dirty) work tree — an agent that
#             remembers nothing.
#
# A task dir holds instruction.md and check.sh (exit 0 = pass, run in the
# work tree), and optionally setup.sh (fills $WORK; may write $OUT/prompt.md
# to replace instruction.md), on-kill.sh (after each kill), teardown.sh, and
# a `timeout` file (seconds). Every hook sees TASK, WORK, OUT.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
RUNNER=${HOLT_RUNNER:-$REPO/target/debug/holt-runner}
TASK= ARM= OUT= MODEL= KILL_AFTER=20:60 KILLS=1 TIMEOUT=
while [ $# -gt 0 ]; do
  case $1 in
    --task) TASK=$(cd "$2" && pwd); shift 2 ;;
    --arm) ARM=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --model) MODEL=$2; shift 2 ;;
    --kill-after) KILL_AFTER=$2; shift 2 ;;
    --kills) KILLS=$2; shift 2 ;;
    --timeout) TIMEOUT=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$TASK" ] && [ -n "$ARM" ] && [ -n "$OUT" ] && [ -n "$MODEL" ] \
  || { echo "usage: run-task.sh --task DIR --arm baseline|resume|cold --out DIR --model P/M" >&2; exit 2; }
case $ARM in baseline|resume|cold) ;; *) echo "unknown arm $ARM" >&2; exit 2 ;; esac
[ -x "$RUNNER" ] || { echo "no holt-runner at $RUNNER (cargo build -p holt-runner)" >&2; exit 2; }
if [ -z "$TIMEOUT" ]; then
  TIMEOUT=$(cat "$TASK/timeout" 2>/dev/null || echo 1800)
fi
[ "$ARM" = baseline ] && KILLS=0

rm -rf "$OUT"
mkdir -p "$OUT/work"
OUT=$(cd "$OUT" && pwd)
WORK=$OUT/work
export TASK WORK OUT

# A task that ships `shell.sh` wants every command run through it instead of
# the login shell (tb2 tasks forward into their container).
SHELL_ARGS=()
[ -f "$TASK/shell.sh" ] && SHELL_ARGS=(--shell "$TASK/shell.sh")

hook() { if [ -f "$TASK/$1" ]; then bash "$TASK/$1"; fi; }
trap 'hook teardown.sh || true' EXIT

hook setup.sh
# Step checkpoints need a Git work tree; give every task one.
if ! git -C "$WORK" rev-parse --git-dir >/dev/null 2>&1; then
  git -C "$WORK" init -q
  git -C "$WORK" add -A
  git -C "$WORK" -c user.email=eval@holt -c user.name=eval commit -qm setup --allow-empty
fi
PROMPT=$OUT/prompt.md
[ -f "$PROMPT" ] || cp "$TASK/instruction.md" "$PROMPT"

kill_delay() {
  case $KILL_AFTER in
    *:*) local lo=${KILL_AFTER%:*} hi=${KILL_AFTER#*:}
         echo $(( lo + RANDOM % (hi - lo + 1) )) ;;
    *) echo "$KILL_AFTER" ;;
  esac
}

# Kill the shell process groups a killed run left behind — what Holt's own
# launch does for the resume arm, done by hand for the cold arm.
reap() {
  local groups=$1/shell-groups pgid
  [ -d "$groups" ] || return 0
  for file in "$groups"/*.json; do
    [ -e "$file" ] || continue
    pgid=$(basename "$file" .json)
    kill -9 -- "-$pgid" 2>/dev/null || true
  done
}

started=$(date +%s)
run=0 kills=0 data=$OUT/data-1
echo "[]" > "$OUT/kills.json"
while :; do
  run=$((run + 1))
  left=$(( TIMEOUT - ($(date +%s) - started) ))
  if [ "$left" -le 0 ]; then break; fi
  if [ "$run" -gt 1 ] && [ "$ARM" = resume ]; then
    "$RUNNER" --resume --data-dir "$data" --timeout-sec "$left" \
      ${SHELL_ARGS[@]+"${SHELL_ARGS[@]}"} \
      > "$OUT/run-$run.json" 2> "$OUT/run-$run.err" &
  else
    "$RUNNER" --cwd "$WORK" --prompt-file "$PROMPT" --model "$MODEL" \
      --data-dir "$data" --timeout-sec "$left" ${SHELL_ARGS[@]+"${SHELL_ARGS[@]}"} \
      > "$OUT/run-$run.json" 2> "$OUT/run-$run.err" &
  fi
  pid=$!
  killed=0
  if [ "$kills" -lt "$KILLS" ]; then
    delay=$(kill_delay)
    for _ in $(seq 1 "$delay"); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid"
      killed=1
      kills=$((kills + 1))
      python3 - "$OUT/kills.json" "$run" "$delay" <<'EOF'
import json, sys
path, run, delay = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
kills = json.load(open(path))
kills.append({"run": run, "afterSecs": delay})
json.dump(kills, open(path, "w"))
EOF
    fi
  fi
  wait "$pid" || true
  [ "$killed" = 1 ] || break
  hook on-kill.sh
  if [ "$ARM" = cold ]; then
    reap "$data"
    data=$OUT/data-$((run + 1))
  fi
done
wall=$(( $(date +%s) - started ))

pass=false
if (cd "$WORK" && bash "$TASK/check.sh") > "$OUT/check.log" 2>&1; then pass=true; fi

python3 - "$OUT" "$(basename "$TASK")" "$ARM" "$MODEL" "$wall" "$pass" <<'EOF'
import glob, json, os, sys
out, task, arm, model, wall, passed = sys.argv[1:]

def load(path):
    try:
        return json.load(open(path))
    except (OSError, ValueError):
        return None

runs = []
for path in sorted(glob.glob(os.path.join(out, "run-*.json")),
                   key=lambda p: int(p.rsplit("-", 1)[1].split(".")[0])):
    runs.append(load(path))

def history(data_dir):
    """Committed model calls and tool calls across every process on one data dir."""
    calls, tokens, requests, intents = [], 0, 0, 0
    path = os.path.join(data_dir, "history", "run.jsonl")
    if not os.path.exists(path):
        return None
    for line in open(path):
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        message = entry.get("entry", {})
        if entry.get("kind") == "message" and message.get("role") == "assistant":
            requests += 1
            tokens += (message.get("usage") or {}).get("totalTokens", 0)
            for block in message.get("content", []):
                if block.get("type") == "toolCall":
                    calls.append(block.get("arguments", {}).get("command"))
        elif entry.get("kind") == "toolIntent":
            intents += 1
    return {"tokens": tokens, "requests": requests, "intents": intents, "commands": calls}

dirs = [history(d) for d in sorted(glob.glob(os.path.join(out, "data-*")))]
dirs = [d for d in dirs if d]
commands = [c for d in dirs for c in d["commands"] if c]
seen, repeated = set(), 0
for command in commands:
    if command in seen:
        repeated += 1
    seen.add(command)
last = next((r for r in reversed(runs) if r), None)
result = {
    "task": task,
    "arm": arm,
    "model": model,
    "pass": passed == "true",
    "wallSecs": int(wall),
    "kills": load(os.path.join(out, "kills.json")) or [],
    "runs": len(runs),
    "finalStatus": last and last.get("status"),
    "relaunched": [r.get("relaunched") for r in runs if r and r.get("resumed")],
    "totalTokens": sum(d["tokens"] for d in dirs),
    "modelRequests": sum(d["requests"] for d in dirs),
    "toolCalls": sum(len(d["commands"]) for d in dirs),
    "repeatedCommands": repeated,
}
json.dump(result, open(os.path.join(out, "result.json"), "w"), indent=2)
print(json.dumps(result))
EOF
