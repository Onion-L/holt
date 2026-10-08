#!/usr/bin/env bash
# Verifies that the TypeScript oracle under pi-core/ matches the committed
# source manifest (oracle/pi-core-manifest.txt). The oracle tree is git-
# ignored, so the manifest is the tamper-evidence record: any local edit to
# an oracle file, added file, or removed file fails this script.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/oracle/pi-core-manifest.txt"

if [[ ! -f "$manifest" ]]; then
	echo "error: missing oracle manifest at $manifest" >&2
	echo "       generate it with scripts/generate-oracle-manifest.sh" >&2
	exit 1
fi
if [[ ! -d "$repo_root/pi-core" ]]; then
	echo "error: pi-core/ is absent; restore the oracle from upstream v0.84.4" >&2
	exit 1
fi

computed="$(cd "$repo_root" && scripts/generate-oracle-manifest.sh --stdout)"
committed="$(cat "$manifest")"

if [[ "$computed" == "$committed" ]]; then
	count="$(grep -c . "$manifest")"
	echo "oracle OK: $count files match oracle/pi-core-manifest.txt (upstream v0.84.4)"
	exit 0
fi

echo "error: pi-core/ does not match oracle/pi-core-manifest.txt" >&2
diff <(echo "$committed") <(echo "$computed") | head -40 >&2 || true
echo "Either restore the oracle sources or regenerate the manifest with" >&2
echo "scripts/generate-oracle-manifest.sh and review the diff." >&2
exit 1
