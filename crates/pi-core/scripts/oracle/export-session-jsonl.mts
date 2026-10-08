/**
 * TypeScript oracle golden generator for the pi-agent-core JSONL v4
 * session format.
 *
 * Drives the real codec/storage/session modules with a fixed clock and id
 * generator and freezes the resulting JSONL bytes (plus individual codec
 * lines) under tests/goldens/session-jsonl/ for the Rust port to
 * byte-compare.
 *
 * Run:   node --experimental-strip-types scripts/oracle/export-session-jsonl.mts --write
 * Check: node --experimental-strip-types scripts/oracle/export-session-jsonl.mts
 */

import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const FIXED_NOW = 1_700_000_000_000;
const realNow = Date.now;
Date.now = () => FIXED_NOW;
try {
	const { JsonlSessionStorage } = await import("../../pi-core/agent/src/harness/session/jsonl/storage.ts");
	const { Session } = await import("../../pi-core/agent/src/harness/session/session.ts");
	const { encodeHeader, encodeMutation } = await import(
		 "../../pi-core/agent/src/harness/session/jsonl/codec.ts"
	);

	// Minimal node:fs-backed FileSystem for the storage layer.
	const fs = {
		async absolutePath(path) {
			return { ok: true, value: resolvePath(path) };
		},
		async joinPath(parts) {
			return { ok: true, value: parts.reduce((acc, part) => join(acc, part)) };
		},
		async readTextFile(path) {
			try {
				return { ok: true, value: await import("node:fs/promises").then((m) => m.readFile(path, "utf8")) };
			} catch (error) {
				return { ok: false, error: { code: "ENOENT", message: String(error) } };
			}
		},
		async writeFile(path, content) {
			const m = await import("node:fs/promises");
			await m.mkdir(dirname(path), { recursive: true });
			await m.writeFile(path, content);
			return { ok: true, value: undefined };
		},
		async appendFile(path, content) {
			const m = await import("node:fs/promises");
			await m.mkdir(dirname(path), { recursive: true });
			await m.appendFile(path, content);
			return { ok: true, value: undefined };
		},
		async renameFile(source, destination) {
			const m = await import("node:fs/promises");
			await m.rename(source, destination);
			return { ok: true, value: undefined };
		},
		async remove(path, options = {}) {
			const m = await import("node:fs/promises");
			await m.rm(path, { recursive: options.recursive ?? false, force: options.force ?? false });
			return { ok: true, value: undefined };
		},
		async fileInfo(path) {
			const m = await import("node:fs/promises");
			const stats = await m.stat(path);
			return {
				ok: true,
				value: {
					name: nodePath.basename(path),
					path,
					kind: stats.isFile() ? "file" : stats.isDirectory() ? "directory" : "symlink",
					size: stats.size,
					mtimeMs: stats.mtimeMs,
				},
			};
		},
	};

	function resolvePath(path) {
		const { resolve } = require("node:path");
		return resolve(path);
	}
	function dirname(path) {
		return path.slice(0, Math.max(path.lastIndexOf("/"), 0)) || ".";
	}
	function require(specifier) {
		return specifier === "node:path" ? nodePath : undefined;
	}
	const nodePath = await import("node:path");

	const write = process.argv.includes("--write");
	const goldenDir = new URL("../../tests/goldens/session-jsonl/", import.meta.url);
	const outputs = new Map<string, string>();

	// -----------------------------------------------------------------------
	// Codec line goldens
	// -----------------------------------------------------------------------
	outputs.set(
		"header-line.jsonl",
		encodeHeader({
			kind: "header",
			version: 4,
			id: "session",
			createdAt: 1_700_000_000_000,
			cwd: "/workspace/project",
			parentSessionId: "parent",
			metadata: { owner: "agent", nested: { enabled: true }, values: [1, null, "two"] },
		}),
	);
	outputs.set(
		"header-legacy.jsonl",
		encodeHeader({
			kind: "header",
			version: 4,
			id: "legacy-child",
			createdAt: 1_700_000_000_001,
			cwd: "/workspace/project",
			legacyParentSessionPath: "/sessions/missing-parent.jsonl",
		}),
	);

	// -----------------------------------------------------------------------
	// Full session file golden driven through the public Session API
	// -----------------------------------------------------------------------
	const root = join(tmpdir(), `pi-session-golden-${FIXED_NOW}`);
	rmSync(root, { recursive: true, force: true });
	mkdirSync(root, { recursive: true });
	const sessionPath = join(root, "session.jsonl");

	const storage = await JsonlSessionStorage.create(fs, sessionPath, {
		kind: "header",
		version: 4,
		id: "golden-session",
		createdAt: FIXED_NOW,
		cwd: "/workspace/project",
	});
	let nextEntryId = 0;
	const session = new Session(storage, { idGenerator: { next: () => `entry-${++nextEntryId}` } });

	await session.appendMessage({
		role: "user",
		content: [{ type: "text", text: "hello" }],
		timestamp: FIXED_NOW,
	});
	await session.appendMessage({
		role: "assistant",
		content: [
			{ type: "thinking", thinking: "hmm", thinkingSignature: "sig" },
			{ type: "text", text: "hi there" },
			{ type: "toolCall", id: "call-1", name: "bash", arguments: { command: "ls" } },
		],
		api: "anthropic-messages",
		provider: "anthropic",
		model: "claude-sonnet-4-5",
		usage: {
			input: 10,
			output: 20,
			cacheRead: 5,
			cacheWrite: 0,
			totalTokens: 35,
			cost: { input: 0.1, output: 0.2, cacheRead: 0.05, cacheWrite: 0, total: 0.35 },
		},
		stopReason: "toolUse",
		timestamp: FIXED_NOW,
	});
	await session.appendMessage({
		role: "toolResult",
		toolCallId: "call-1",
		toolName: "bash",
		content: [{ type: "text", text: "files" }],
		details: { exitCode: 0 },
		isError: false,
		timestamp: FIXED_NOW,
	});
	await session.appendCustomEntry("note", { text: "a custom note" });
	await session.appendEntry(
		{
			type: "model_change",
			id: "model-1",
			provider: "openai",
			modelId: "gpt-5",
		},
		"main",
	);
	await session.appendEntry(
		{ type: "thinking_level_change", id: "thinking-1", thinkingLevel: "high" },
		"main",
	);
	await session.appendEntry(
		{ type: "active_tools_change", id: "tools-1", activeToolNames: ["bash", "read"] },
		"main",
	);
	await session.appendRecord({
		type: "operation_started",
		id: "run-1",
		lane: "main",
		sourceLeafId: null,
		intent: { kind: "run", originalPrompt: [], initialMessages: [] },
	});
	await session.appendRecord({
		type: "usage",
		id: "usage-1",
		lane: "main",
		usage: {
			input: 10,
			output: 20,
			cacheRead: 5,
			cacheWrite: 0,
			totalTokens: 35,
			cost: { input: 0.1, output: 0.2, cacheRead: 0.05, cacheWrite: 0, total: 0.35 },
		},
		cause: "assistant",
		runId: "run-1",
		entryId: "entry-2",
		attempt: 1,
		stopReason: "toolUse",
	});
	await session.appendRecord({
		type: "operation_finished",
		id: "finish-1",
		lane: "main",
		runId: "run-1",
		outcome: "completed",
	});
	const threadLeaf = "entry-1";
	await session.createLane("thread", threadLeaf);
	await session.appendEntry({ type: "custom", id: "thread-note", customType: "thread" }, "thread");
	await session.setName("Golden session");
	await session.setLabel("entry-1", "checkpoint");

	const sessionBytes = readFileSync(sessionPath, "utf8");
	outputs.set("session.jsonl", sessionBytes);

	// Individual mutation lines from the same session for focused checks.
	const lines = sessionBytes.split("\n").filter((line) => line.length > 0);
	outputs.set("mutation-entry-message.jsonl", lines[1] + "\n");
	outputs.set("mutation-entry-custom.jsonl", lines[4] + "\n");
	outputs.set("mutation-record-usage.jsonl", lines[8] + "\n");
	outputs.set("mutation-lane.jsonl", lines[11] + "\n");
	outputs.set("mutation-fact-name.jsonl", lines[12] + "\n");
	outputs.set("mutation-fact-label.jsonl", lines[13] + "\n");

	// A compaction entry with retained tail and usage, written through the
	// storage layer directly.
	const compactionPath = join(root, "compaction.jsonl");
	const compactionStorage = await JsonlSessionStorage.create(fs, compactionPath, {
		kind: "header",
		version: 4,
		id: "compaction-session",
		createdAt: FIXED_NOW,
		cwd: "/workspace/project",
	});
	await compactionStorage.appendEntry(
		{
			type: "compaction",
			id: "compact-1",
			summary: "earlier conversation",
			retainedTail: [{ role: "user", content: "kept", timestamp: FIXED_NOW }],
			tokensBefore: 1234,
			details: { reason: "manual" },
			usage: {
				input: 1,
				output: 2,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 3,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
		},
		"main",
	);
	outputs.set("compaction.jsonl", readFileSync(compactionPath, "utf8"));

	// Branch summary entry.
	const branchPath = join(root, "branch.jsonl");
	const branchStorage = await JsonlSessionStorage.create(fs, branchPath, {
		kind: "header",
		version: 4,
		id: "branch-session",
		createdAt: FIXED_NOW,
		cwd: "/workspace/project",
	});
	await branchStorage.appendEntry(
		{
			type: "branch_summary",
			id: "branch-1",
			fromId: "abandoned-1",
			summary: "abandoned branch work",
		},
		"main",
	);
	outputs.set("branch-summary.jsonl", readFileSync(branchPath, "utf8"));

	// Direct encodeMutation checks for cleared facts.
	outputs.set(
		"mutation-fact-cleared.jsonl",
		encodeMutation({ kind: "fact", seq: 1, fact: "name", name: undefined }),
	);

	if (write) {
		mkdirSync(goldenDir, { recursive: true });
		for (const [name, content] of outputs) {
			writeFileSync(new URL(name, goldenDir), content);
		}
		console.log(`wrote ${outputs.size} golden files to tests/goldens/session-jsonl/`);
	} else {
		let mismatch = 0;
		for (const [name, content] of outputs) {
			let expected: string;
			try {
				expected = readFileSync(new URL(name, goldenDir), "utf8");
			} catch {
				console.error(`missing golden: ${name}`);
				mismatch += 1;
				continue;
			}
			if (expected !== content) {
				console.error(`golden mismatch: ${name}`);
				mismatch += 1;
			}
		}
		if (mismatch > 0) process.exitCode = 1;
		else console.log(`verified ${outputs.size} golden files`);
	}

	rmSync(root, { recursive: true, force: true });
} finally {
	Date.now = realNow;
}
