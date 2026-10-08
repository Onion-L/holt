#!/usr/bin/env bash
# Generates the pi-core source manifest: one "sha256  path" line per oracle
# file (package-lock.json and node_modules are content-addressed dependency
# pinning, not oracle behavior, so only package.json version fields matter
# and the lockfile is excluded).
#
# With --stdout, prints the manifest instead of writing the file (used by
# scripts/verify-oracle.sh).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$repo_root/oracle/pi-core-manifest.txt"

manifest() {
	cd "$repo_root"
	# Record the upstream package versions first so the manifest names the
	# exact release it pins.
	for pkg in telemetry ai agent; do
		version="$(node -p "require('./pi-core/$pkg/package.json').version" 2>/dev/null ||
			sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "pi-core/$pkg/package.json" | head -1 || true)"
		echo "# pi-core/$pkg version: ${version:-unknown}"
	done
	find pi-core -type f \
		! -path "pi-core/*/node_modules/*" \
		! -name "package-lock.json" \
		! -name ".DS_Store" \
		-print0 |
		sort -z |
		xargs -0 shasum -a 256
}

if [[ "${1:-}" == "--stdout" ]]; then
	manifest
else
	manifest >"$target"
	echo "wrote $target ($(grep -vc '^#' "$target") files)"
fi
