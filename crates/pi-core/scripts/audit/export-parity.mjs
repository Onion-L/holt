#!/usr/bin/env node
/**
 * Export-parity audit between the TypeScript oracle packages and the Rust
 * port.
 *
 * Enumerates every public export of `pi-core/{telemetry,ai,agent}/src`
 * (statically, via the TypeScript compiler AST from the oracle's own
 * `typescript` dependency) and checks each name against the classified
 * manifest `scripts/audit/export-parity.json`:
 *
 *   mapped     a public Rust symbol with the recorded name exists under
 *              `src/` (verified by this script and by
 *              `tests/export_parity.rs`);
 *   partial    the runtime capability exists in Rust but the TS public
 *              surface is only partly exposed (reason required);
 *   exception  a TypeScript language, bundler, or schema-level construct
 *              with no Rust counterpart by design (reason required).
 *
 * Fails when a TS export has no manifest entry or a `todo` status, when a
 * manifest entry no longer matches any TS export, or when a `mapped` entry
 * has no matching `pub` Rust symbol. This is the tripwire that keeps
 * `MIGRATION.md` honest: new oracle exports cannot appear without an
 * explicit Rust-side decision.
 *
 * Usage:
 *   node scripts/audit/export-parity.mjs            # validate
 *   node scripts/audit/export-parity.mjs --update   # sync TS side into the
 *                              manifest, keeping existing classifications
 */

import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = fileURLToPath(new URL("../..", import.meta.url));
const manifestPath = path.join(repoRoot, "scripts/audit/export-parity.json");
const packages = ["telemetry", "ai", "agent"];
const packageRoots = Object.fromEntries(
  packages.map((pkg) => [pkg, path.join(repoRoot, "pi-core", pkg, "src")]),
);
const rustRoot = path.join(repoRoot, "src");

// The oracle pins `typescript` for its own tooling; reuse it so the export
// scan follows exactly the syntax the oracle compiles.
const require = createRequire(
  path.join(repoRoot, "pi-core/agent/package.json"),
);
const ts = require("typescript");

function listTsFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir).sort()) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      out.push(...listTsFiles(full));
    } else if (entry.endsWith(".ts") && !entry.endsWith(".d.ts")) {
      out.push(full);
    }
  }
  return out;
}

/** Collect the exported names of one source file via the TS AST. */
function exportedNames(file) {
  const source = ts.createSourceFile(
    file,
    readFileSync(file, "utf8"),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ false,
    ts.ScriptKind.TS,
  );
  const names = [];
  const add = (name) => {
    if (name && name !== "default") names.push(name);
  };
  const bindingIdentifiers = (node) => {
    if (ts.isIdentifier(node)) return [node.text];
    if (ts.isObjectBindingPattern(node) || ts.isArrayBindingPattern(node)) {
      return node.elements.flatMap((el) =>
        ts.isBindingElement(el) ? bindingIdentifiers(el.name) : [],
      );
    }
    return [];
  };
  const hasExportModifier = (mods) =>
    Boolean(mods?.some((m) => m.kind === ts.SyntaxKind.ExportKeyword));
  for (const statement of source.statements) {
    if (ts.isExportDeclaration(statement)) {
      const { exportClause, moduleSpecifier } = statement;
      if (!exportClause) continue; // `export * from` adds no new names here
      if (ts.isNamedExports(exportClause)) {
        for (const element of exportClause.elements) {
          // `export { a as b }` exports `b`; with a module specifier the
          // local name `a` is not a member of this module's own surface.
          add(element.name.text);
        }
      } else if (ts.isNamespaceExport(exportClause)) {
        add(exportClause.name.text);
      }
      void moduleSpecifier;
    } else if (ts.isExportAssignment(statement)) {
      continue;
    } else if (hasExportModifier(statement.modifiers)) {
      if (
        ts.isClassDeclaration(statement) ||
        ts.isFunctionDeclaration(statement) ||
        ts.isInterfaceDeclaration(statement) ||
        ts.isTypeAliasDeclaration(statement) ||
        ts.isEnumDeclaration(statement) ||
        ts.isModuleDeclaration(statement)
      ) {
        if (statement.name) add(statement.name.text);
      } else if (ts.isVariableStatement(statement)) {
        for (const decl of statement.declarationList.declarations) {
          for (const id of bindingIdentifiers(decl.name)) add(id);
        }
      }
    }
  }
  return names;
}

/** (package, name) -> sorted defining/re-exporting file list. */
function scanTypeScriptExports() {
  const found = new Map();
  for (const pkg of packages) {
    for (const file of listTsFiles(packageRoots[pkg])) {
      const rel = path.relative(packageRoots[pkg], file).replaceAll("\\", "/");
      for (const name of exportedNames(file)) {
        const key = `${pkg}\u0000${name}`;
        if (!found.has(key)) found.set(key, { pkg, name, files: [] });
        found.get(key).files.push(rel);
      }
    }
  }
  for (const record of found.values()) record.files.sort();
  return found;
}

/** Every `pub` item name in the Rust sources (single flat namespace). */
function collectRustPublicItems() {
  const names = new Set();
  const walk = (dir) => {
    for (const entry of readdirSync(dir).sort()) {
      const full = path.join(dir, entry);
      if (statSync(full).isDirectory()) walk(full);
      else if (entry.endsWith(".rs")) {
        const text = readFileSync(full, "utf8");
        const itemPattern =
          /\bpub(?:\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|trait|type|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)/g;
        for (const m of text.matchAll(itemPattern)) names.add(m[1]);
        // `pub use a::b as c;` / `pub use a::{b, c as d};`
        const usePattern = /\bpub(?:\([^)]*\))?\s+use\s+([^;]+);/g;
        for (const m of text.matchAll(usePattern)) {
          for (const part of m[1].split(/[{},]/)) {
            const asMatch = part.trim().match(/(\S+)\s+as\s+([A-Za-z_][A-Za-z0-9_]*)$/);
            if (asMatch) {
              names.add(asMatch[2]);
            } else {
              const seg = part.trim().split("::").pop()?.trim();
              if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(seg ?? "")) names.add(seg);
            }
          }
        }
      }
    }
  };
  walk(rustRoot);
  return names;
}

function loadManifest() {
  return JSON.parse(readFileSync(manifestPath, "utf8"));
}

function camelToSnake(name) {
  // Word-splitting form: `azureOpenAIResponsesProvider` ->
  // `azure_open_ai_responses_provider`.
  return name
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/([A-Z]+)([A-Z][a-z])/g, "$1_$2")
    .toLowerCase();
}

function camelToSnakeAcronymMerged(name) {
  // Acronym-merging form: `azureOpenAIResponsesProvider` ->
  // `azure_openai_responses_provider` (how the Rust port spells most
  // OpenAI/AI identifiers).
  return name
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/([A-Z])([A-Z][a-z])/g, "$1$2")
    .toLowerCase();
}

const compact = (name) => name.replaceAll("_", "").toLowerCase();

/**
 * Fill `todo` entries that unambiguously match a public Rust symbol:
 * exact name, then the two camelToSnake spellings, then a
 * punctuation-insensitive comparison. Never reclassifies a decided entry.
 */
function autoMap(manifest, tsExports, rustItems) {
  const byExact = rustItems;
  const bySnake = new Map();
  const byCompact = new Map();
  for (const rust of rustItems) {
    byCompact.set(compact(rust), rust);
  }
  let filled = 0;
  for (const { pkg, name } of tsExports.values()) {
    const entry = manifest.entries[pkg]?.[name];
    if (!entry || entry.status !== "todo") continue;
    const candidates = [
      byExact.has(name) ? name : undefined,
      byExact.has(camelToSnake(name)) ? camelToSnake(name) : undefined,
      byExact.has(camelToSnakeAcronymMerged(name))
        ? camelToSnakeAcronymMerged(name)
        : undefined,
      byCompact.get(compact(name)),
    ].filter(Boolean);
    const unique = [...new Set(candidates)];
    if (unique.length === 1) {
      entry.status = "mapped";
      entry.rust = unique[0];
      filled += 1;
    }
  }
  return filled;
}

function validate({ manifest, tsExports, rustItems, update }) {
  const problems = [];
  const stats = { mapped: 0, partial: 0, exception: 0, todo: 0 };
  const manifestEntries = manifest.entries ?? {};

  for (const pkg of packages) {
    manifestEntries[pkg] ??= {};
  }
  for (const pkg of packages) {
    for (const name of Object.keys(manifestEntries[pkg])) {
      if (!packages.includes(pkg) || typeof manifestEntries[pkg][name] !== "object") {
        problems.push(`malformed manifest entry: ${pkg}/${name}`);
      }
    }
  }

  // TS side: every export must be classified.
  for (const { pkg, name, files } of tsExports.values()) {
    const entry = manifestEntries[pkg]?.[name];
    if (entry === undefined) {
      if (update) {
        manifestEntries[pkg][name] = { status: "todo", files };
        problems.push(`added unclassified TS export: ${pkg}/${name}`);
      } else {
        problems.push(`unclassified TS export: ${pkg}/${name} (run with --update)`);
      }
      continue;
    }
    if (update) entry.files = files;
    stats[entry.status] = (stats[entry.status] ?? 0) + 1;
    if (entry.status === "todo") {
      problems.push(`unclassified TS export: ${pkg}/${name} (status "todo")`);
    }
    if (
      (entry.status === "partial" || entry.status === "exception") &&
      !entry.reason
    ) {
      problems.push(`missing reason for ${entry.status} export: ${pkg}/${name}`);
    }
    if (entry.status === "mapped" && !entry.rust) {
      problems.push(`mapped export without rust symbol: ${pkg}/${name}`);
    }
  }

  // Manifest side: drop or flag entries with no TS counterpart.
  for (const pkg of packages) {
    for (const [name, entry] of Object.entries(manifestEntries[pkg])) {
      if (!tsExports.has(`${pkg}\u0000${name}`)) {
        problems.push(`stale manifest entry (no such TS export): ${pkg}/${name} (${entry.status})`);
      }
    }
  }

  // Rust side: mapped symbols must exist as public items.
  if (!update) {
    for (const pkg of packages) {
      for (const [name, entry] of Object.entries(manifestEntries[pkg])) {
        if (entry.status === "mapped" && entry.rust && !rustItems.has(entry.rust)) {
          problems.push(`mapped export has no pub Rust symbol "${entry.rust}": ${pkg}/${name}`);
        }
      }
    }
  }

  return { problems, stats };
}

const update = process.argv.includes("--update");
const autoMapFlag = process.argv.includes("--auto-map");
const manifest = loadManifest();
const tsExports = scanTypeScriptExports();
const rustItems = collectRustPublicItems();
if (autoMapFlag) {
  const filled = autoMap(manifest, tsExports, rustItems);
  console.error(`auto-mapped ${filled} export(s)`);
}
const { problems, stats } = validate({ manifest, tsExports, rustItems, update });

if (update || autoMapFlag) {
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
  console.error(`manifest updated: ${manifestPath}`);
}

for (const problem of problems) console.error(`FAIL: ${problem}`);
const perPackage = {};
for (const { pkg } of tsExports.values()) {
  perPackage[pkg] = (perPackage[pkg] ?? 0) + 1;
}
console.error(
  `ts exports: ${tsExports.size} (${Object.entries(perPackage)
    .map(([pkg, n]) => `${pkg} ${n}`)
    .join(", ")}); statuses: ` +
    Object.entries(stats)
      .filter(([, n]) => n > 0)
      .map(([status, n]) => `${status} ${n}`)
      .join(", "),
);
if (problems.length > 0) {
  console.error(`export-parity: ${problems.length} problem(s)`);
  process.exit(1);
}
console.error("export-parity: OK");
