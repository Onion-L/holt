/**
 * TypeScript oracle exporter for the agent telemetry schemas
 * (`harness/telemetry.ts`).
 *
 * Serializes AI_TELEMETRY_SCHEMA and HARNESS_TELEMETRY_SCHEMA so the Rust
 * port can embed the exact same definitions.
 *
 * Run: node --experimental-strip-types scripts/oracle/export-agent-telemetry-schemas.mts --write
 */

import { writeFileSync, mkdirSync } from "node:fs";

const { AI_TELEMETRY_SCHEMA, HARNESS_TELEMETRY_SCHEMA } = await import(
	"../../pi-core/agent/src/harness/telemetry.ts"
);

const payload = JSON.stringify({ ai: AI_TELEMETRY_SCHEMA, harness: HARNESS_TELEMETRY_SCHEMA }, null, "\t");

if (process.argv.includes("--write")) {
	mkdirSync(new URL("../../src/agent/harness/data/", import.meta.url), { recursive: true });
	writeFileSync(
		new URL("../../src/agent/harness/data/telemetry-schemas.json", import.meta.url),
		payload + "\n",
	);
	console.log("wrote src/agent/harness/data/telemetry-schemas.json");
} else {
	console.log(payload);
}
