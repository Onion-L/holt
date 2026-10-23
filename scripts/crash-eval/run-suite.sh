#!/usr/bin/env bash
# Every task × arm × repeat of the crash-injection eval, a few at a time.
#
#   run-suite.sh --tasks DIR --out DIR --model P/M [--arms "baseline resume cold"]
#                [--repeats N] [--jobs N] [--kill-after S | S1:S2] [--kills N]
#
# DIR holds task dirs (run-task.sh's format; tb2-task.sh makes them from
# Terminal-Bench 2). Results land in OUT/<task>/<arm>/<repeat>/result.json;
# summarize.py reads them.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TASKS= OUT= MODEL= ARMS="baseline resume cold" REPEATS=1 JOBS=2 KILL_AFTER=20:60 KILLS=1 SKIP_EXISTING=0
while [ $# -gt 0 ]; do
  case $1 in
    --tasks) TASKS=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --model) MODEL=$2; shift 2 ;;
    --arms) ARMS=$2; shift 2 ;;
    --repeats) REPEATS=$2; shift 2 ;;
    --jobs) JOBS=$2; shift 2 ;;
    --kill-after) KILL_AFTER=$2; shift 2 ;;
    --kills) KILLS=$2; shift 2 ;;
    --skip-existing) SKIP_EXISTING=1; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$TASKS" ] && [ -n "$OUT" ] && [ -n "$MODEL" ] \
  || { echo "usage: run-suite.sh --tasks DIR --out DIR --model P/M" >&2; exit 2; }
mkdir -p "$OUT"
for task in "$TASKS"/*/; do
  [ -f "$task/check.sh" ] || continue
  for arm in $ARMS; do
    for rep in $(seq 1 "$REPEATS"); do
      if [ "$SKIP_EXISTING" = 1 ] && [ -f "$OUT/$(basename "$task")/$arm/$rep/result.json" ]; then
        echo "skip: $(basename "$task")/$arm/$rep" >&2
        continue
      fi
      printf '%s\0%s\0%s\0' "${task%/}" "$arm" "$OUT/$(basename "$task")/$arm/$rep"
    done
  done
done | xargs -0 -n 3 -P "$JOBS" sh -c '
  mkdir -p "$(dirname "$6")"
  "$0" --task "$4" --arm "$5" --out "$6" --model "$1" --kill-after "$2" --kills "$3" \
    > "$6.log" 2>&1 || echo "failed: $6 (see $6.log)" >&2
  tail -n 1 "$6.log"
' "$HERE/run-task.sh" "$MODEL" "$KILL_AFTER" "$KILLS"
