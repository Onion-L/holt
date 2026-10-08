/**
 * TypeScript oracle golden generator for the `pi-ai` CLI bin entry
 * (pi-core/ai/src/cli.ts).
 *
 * Drives the real cli.ts as a child process and freezes its observable
 * stdout/stderr plus the auth.json bytes under tests/goldens/ai/ for the
 * Rust port to byte-compare.
 *
 * - help/list: plain `node cli.ts <command>` runs (no filesystem access).
 * - menu/select: interactive prompts fed scripted stdin; the selection
 *   fails validation, so no network is touched.
 * - login: a `--worker-login` mode installs a scripted `fetch` (GitHub
 *   Copilot device flow) before importing cli.ts, so the full login
 *   completes offline in a temp cwd and the real saveAuth writes auth.json.
 *
 * Run:   node scripts/oracle/generate-cli-goldens.mts --write
 * Check: node scripts/oracle/generate-cli-goldens.mts
 */

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = dirname(dirname(dirname(fileURLToPath(import.meta.url))));
const cliTs = join(repoRoot, "pi-core/ai/src/cli.ts");
const goldensDir = join(repoRoot, "tests/goldens/ai");
const write = process.argv.includes("--write");

if (!process.argv.includes("--worker-login")) {
	/** Runs cli.ts with optional stdin; returns { stdout, stderr, status }. */
	const runCli = (args: string[], stdin: string, cwd: string) => {
		const result = spawnSync(process.execPath, [cliTs, ...args], {
			input: stdin,
			cwd,
			encoding: "utf8",
		});
		if (result.error) throw result.error;
		return { stdout: result.stdout, stderr: result.stderr, status: result.status ?? 0 };
	};

	/** Runs the scripted-fetch login worker in `cwd` with one stdin line. */
	const runLoginWorker = (cwd: string, stdinLine: string) => {
		const self = fileURLToPath(import.meta.url);
		const result = spawnSync(process.execPath, [self, "--worker-login"], {
			input: `${stdinLine}\n`,
			cwd,
			encoding: "utf8",
		});
		if (result.error) throw result.error;
		if (result.status !== 0) {
			throw new Error(`login worker failed (${result.status}): ${result.stderr}`);
		}
		return result.stdout;
	};

	const goldens: Array<[string, string]> = [];
	const freeze = (name: string, content: string) => goldens.push([name, content]);

	const scratch = mkdtempSync(join(tmpdir(), "pi-ai-cli-oracle-"));
	try {
		freeze("cli-help.txt", runCli(["help"], "", scratch).stdout);
		freeze("cli-list.txt", runCli(["list"], "", scratch).stdout);
		// Interactive provider menu with an out-of-range answer: captures the
		// menu lines, the question echo without trailing newline, and the
		// stderr error line.
		const menu = runCli(["login"], "99\n", scratch);
		freeze("cli-menu.txt", menu.stdout);
		freeze("cli-menu-error.txt", menu.stderr);
		// The openai-codex select prompt with an invalid answer: captures the
		// numbered select rendering (no network is reached).
		const select = runCli(["login", "openai-codex"], "5\n", scratch);
		freeze("cli-select.txt", select.stdout);
		freeze("cli-select-error.txt", select.stderr);

		// Full offline login through the real flow + saveAuth.
		const loginDir = mkdtempSync(join(tmpdir(), "pi-ai-cli-oracle-login-"));
		try {
			const stdout = runLoginWorker(loginDir, "");
			freeze("cli-login-copilot-stdout.txt", stdout);
			freeze("cli-login-copilot-auth.json", readFileSync(join(loginDir, "auth.json"), "utf8"));

			// Seed unrelated entries (one oauth, one api_key) and re-run to
			// capture the merge: existing key replaced in place, order kept.
			const seeded = JSON.parse(readFileSync(join(loginDir, "auth.json"), "utf8"));
			seeded["xai"] = { type: "oauth", access: "xai-access", refresh: "xai-refresh", expires: 123 };
			seeded["zai"] = { type: "api_key", key: "zai-key" };
			writeFileSync(join(loginDir, "auth.json"), JSON.stringify(seeded, null, 2));
			runLoginWorker(loginDir, "");
			freeze("cli-login-copilot-merge-auth.json", readFileSync(join(loginDir, "auth.json"), "utf8"));
		} finally {
			rmSync(loginDir, { recursive: true, force: true });
		}
	} finally {
		rmSync(scratch, { recursive: true, force: true });
	}

	// Report / write.
	let mismatches = 0;
	for (const [name, content] of goldens) {
		const path = join(goldensDir, name);
		if (write) {
			mkdirSync(goldensDir, { recursive: true });
			writeFileSync(path, content);
			console.log(`wrote ${path}`);
			continue;
		}
		let expected: string;
		try {
			expected = readFileSync(path, "utf8");
		} catch {
			console.error(`MISSING golden: ${path}`);
			mismatches += 1;
			continue;
		}
		if (expected !== content) {
			console.error(`STALE golden: ${path}`);
			mismatches += 1;
		}
	}
	if (mismatches > 0) process.exit(1);
	if (!write) console.log(`checked ${goldens.length} cli goldens`);
} else {
	// Worker mode: scripted GitHub Copilot device flow, then the real CLI.
	// Runs with cwd = the directory whose auth.json should receive the
	// credential; argv is rewritten so cli.ts sees `login github-copilot`.
	process.argv = [process.argv[0]!, "cli.js", "login", "github-copilot"];
	globalThis.fetch = async (url: string | URL) => {
		const u = String(url);
		let body: string;
		if (u.endsWith("/login/device/code")) {
			body = JSON.stringify({
				device_code: "device-code",
				user_code: "ABCD-EFGH",
				verification_uri: "https://github.com/login/device",
				interval: 1,
				expires_in: 900,
			});
		} else if (u.endsWith("/login/oauth/access_token")) {
			body = JSON.stringify({ access_token: "ghu_refresh_token" });
		} else if (u.includes("/copilot_internal/v2/token")) {
			body = JSON.stringify({
				token: "tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;",
				expires_at: 9999999999,
			});
		} else if (u.endsWith("/models")) {
			body = JSON.stringify({ data: [] });
		} else if (u.includes("/models/") && u.endsWith("/policy")) {
			body = "";
		} else {
			throw new Error(`unexpected fetch URL: ${u}`);
		}
		return new Response(body, {
			status: 200,
			headers: { "content-type": "application/json" },
		});
	};
	await import(cliTs);
}
