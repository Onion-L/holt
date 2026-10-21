#!/usr/bin/env bash
# Wrap a Terminal-Bench 2 task as a crash-eval task dir (run-task.sh's
# format). holt-runner stays on the Mac; the task's container runs with its
# working directory bind-mounted from the work tree, so write/edit land in
# the container and step checkpoints cover its files. Shell commands go
# through `docker exec`, which the prompt asks for.
#
#   tb2-task.sh <terminal-bench-2 task dir> <out task dir>
set -euo pipefail
SRC=$(cd "$1" && pwd)
DEST=$2
mkdir -p "$DEST"
DEST=$(cd "$DEST" && pwd)

field() { sed -n "s/^$1 *= *\"\{0,1\}\([^\"]*\)\"\{0,1\}$/\1/p" "$SRC/task.toml" | head -1; }
IMAGE=$(field docker_image)
[ -n "$IMAGE" ] || { echo "$SRC/task.toml names no docker_image" >&2; exit 1; }
# The [agent] section's timeout.
TIMEOUT=$(awk '/^\[agent\]/{f=1;next} /^\[/{f=0} f&&/timeout_sec/{print int($3)}' "$SRC/task.toml")
WORKDIR=$(awk 'toupper($1)=="WORKDIR"{w=$2} END{print w}' "$SRC/environment/Dockerfile" 2>/dev/null)
WORKDIR=${WORKDIR:-/app}

echo "${TIMEOUT:-1800}" > "$DEST/timeout"
echo "$IMAGE" > "$DEST/image"
cp "$SRC/instruction.md" "$DEST/instruction.md"
printf '%s\n' "$SRC" > "$DEST/source"

# The agent's shell for this task: every bash call lands in the container, so
# the model writes `ls` rather than `docker exec ... bash -lc 'ls'`. The
# container name and workdir are written by setup.sh into $OUT.
cat > "$DEST/shell.sh" <<'EOF'
#!/bin/sh
[ "$1" = -lc ] || exit 97
exec docker exec -w "$(cat "$OUT/workdir")" "$(cat "$OUT/container")" bash -lc "$2"
EOF
chmod +x "$DEST/shell.sh"

cat > "$DEST/setup.sh" <<EOF
set -euo pipefail
c=holt-eval-\$(printf %s "\$OUT" | shasum | cut -c1-12)
echo "\$c" > "\$OUT/container"
echo "$WORKDIR" > "\$OUT/workdir"
docker rm -f "\$c" "\$c-seed" >/dev/null 2>&1 || true
docker create --name "\$c-seed" "$IMAGE" >/dev/null
docker cp "\$c-seed:$WORKDIR/." "\$WORK/" 2>/dev/null || true
docker rm "\$c-seed" >/dev/null
docker run -d --name "\$c" -v "\$WORK:$WORKDIR" -w "$WORKDIR" --entrypoint sleep "$IMAGE" infinity >/dev/null
cat > "\$OUT/prompt.md" <<PROMPT
You are working inside the Linux Docker container \$c. Your shell runs in that container: the working directory $WORKDIR is where the task's files are, and every command you issue executes there. The files you read, write, or edit are the container's files at $WORKDIR.

The task:

\$(cat "\$TASK/instruction.md")
PROMPT
EOF

# A killed run's `docker exec` clients die with it, but what they started
# keeps running in the container; Holt's launch only reaps host process
# groups, so do the container's half here. kill -1 spares PID 1 (the sleep).
cat > "$DEST/on-kill.sh" <<'EOF'
docker exec "$(cat "$OUT/container")" sh -c 'kill -9 -1' >/dev/null 2>&1 || true
EOF

cat > "$DEST/check.sh" <<EOF
set -euo pipefail
c=\$(cat "\$OUT/container")
docker exec "\$c" mkdir -p /tests /logs/verifier
docker cp "$SRC/tests/." "\$c:/tests/"
docker exec -w "$WORKDIR" "\$c" bash /tests/test.sh > "\$OUT/verifier.log" 2>&1 || true
[ "\$(docker exec "\$c" cat /logs/verifier/reward.txt 2>/dev/null | tr -d '[:space:]')" = 1 ]
EOF

cat > "$DEST/teardown.sh" <<'EOF'
[ -f "$OUT/container" ] && docker rm -f "$(cat "$OUT/container")" >/dev/null 2>&1 || true
EOF
echo "$DEST"
