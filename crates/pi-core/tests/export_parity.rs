//! Guard for the TypeScript export-parity manifest.
//!
//! `scripts/audit/export-parity.mjs` is the authoring tool: it enumerates the
//! public exports of the TypeScript oracle and classifies each against this
//! repository. This test re-checks the committed manifest from the Rust side
//! on every `cargo test` run so a removed or renamed public symbol cannot
//! silently invalidate a `mapped` classification, and so no export can sit
//! unclassified.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "scripts/audit/export-parity.json";
const SRC: &str = "src";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("src/ must exist")
        .map(|entry| entry.expect("readable src/ entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `pub` item and `pub use` re-export name in the crate sources.
fn collect_public_symbols() -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(&repo_root().join(SRC), &mut files);
    let mut symbols = BTreeSet::new();
    let item_pattern = regex::Regex::new(
        r"\bpub(?:\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|trait|type|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .unwrap();
    let use_pattern = regex::Regex::new(r"\bpub(?:\([^)]*\))?\s+use\s+([^;]+);").unwrap();
    for file in files {
        let text = fs::read_to_string(&file).expect("readable Rust source");
        for captures in item_pattern.captures_iter(&text) {
            symbols.insert(captures[1].to_string());
        }
        for captures in use_pattern.captures_iter(&text) {
            for part in captures[1].split([',', '{', '}']) {
                let part = part.trim();
                let renamed = match part.split_once(" as ") {
                    Some((_, renamed)) => renamed.trim(),
                    None => part.rsplit("::").next().unwrap_or(part).trim(),
                };
                if is_identifier(renamed) {
                    symbols.insert(renamed.to_string());
                }
            }
        }
    }
    symbols
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

#[test]
fn export_parity_manifest_is_fully_classified_and_mapped_symbols_exist() {
    let manifest_path = repo_root().join(MANIFEST);
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("manifest exists"))
            .expect("manifest is valid JSON");

    let symbols = collect_public_symbols();
    let entries = manifest
        .get("entries")
        .and_then(|entries| entries.as_object())
        .expect("manifest has an entries object");

    let mut problems = Vec::new();
    for (package, entries) in entries {
        let entries = entries.as_object().expect("per-package entry object");
        for (name, entry) in entries {
            let status = entry
                .get("status")
                .and_then(|status| status.as_str())
                .unwrap_or("missing");
            match status {
                "mapped" => {
                    let rust = entry.get("rust").and_then(|rust| rust.as_str());
                    match rust {
                        Some(rust) if symbols.contains(rust) => {}
                        Some(rust) => problems.push(format!(
                            "{package}/{name}: mapped but no pub Rust symbol `{rust}`"
                        )),
                        None => {
                            problems.push(format!("{package}/{name}: mapped without a rust symbol"))
                        }
                    }
                }
                "partial" | "exception" => {
                    if entry
                        .get("reason")
                        .and_then(|reason| reason.as_str())
                        .is_none_or(str::is_empty)
                    {
                        problems.push(format!("{package}/{name}: {status} without a reason"));
                    }
                }
                other => problems.push(format!("{package}/{name}: unclassified status `{other}`")),
            }
        }
    }

    assert!(
        problems.is_empty(),
        "export-parity manifest problems (regenerate with `node scripts/audit/export-parity.mjs`):\n{}",
        problems.join("\n")
    );
}
