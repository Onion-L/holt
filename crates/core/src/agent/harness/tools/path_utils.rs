//! Port of `pi-core/agent/src/harness/tools/path-utils.ts`.

use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::agent::harness::types::ExecutionEnv;

const NARROW_NO_BREAK_SPACE: char = '\u{202f}';

fn normalize_tool_path(path: &str) -> String {
    let normalized: String = path
        .chars()
        .map(|character| match character {
            '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    normalized
        .strip_prefix('@')
        .map(str::to_string)
        .unwrap_or(normalized)
}

/// Port of `resolveToolPath`.
pub async fn resolve_tool_path(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
) -> Result<String, crate::agent::harness::types::FileError> {
    env.absolute_path(&normalize_tool_path(path), None).await
}

/// Port of `resolveReadToolPath`: probes filesystem variants produced by
/// macOS filename normalization and smart apostrophes.
pub async fn resolve_read_tool_path(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
) -> Result<String, crate::agent::harness::types::FileError> {
    let resolved = resolve_tool_path(env, path).await?;
    let nfd: String = resolved.nfd().collect();
    let curly = resolved.replace('\'', "\u{2019}");
    let curly_nfd: String = nfd.replace('\'', "\u{2019}");

    let mut variants: Vec<String> = vec![resolved.clone()];
    variants.push(replace_time_suffix(&resolved));
    variants.push(nfd);
    variants.push(curly);
    variants.push(curly_nfd);

    let mut seen = std::collections::HashSet::new();
    for variant in variants {
        if !seen.insert(variant.clone()) {
            continue;
        }
        if env.exists(&variant, None).await.unwrap_or(false) {
            return Ok(variant);
        }
    }
    Ok(resolved)
}

/// Replaces ` AM.` / ` PM.` with the narrow no-break space variant.
fn replace_time_suffix(path: &str) -> String {
    let bytes: Vec<char> = path.chars().collect();
    let mut out = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        let c = bytes[index];
        if c == ' '
            && index + 3 < bytes.len()
            && matches!(bytes[index + 1], 'A' | 'a' | 'P' | 'p')
            && matches!(bytes[index + 2], 'M' | 'm')
            && bytes[index + 3] == '.'
        {
            out.push(NARROW_NO_BREAK_SPACE);
            out.push(bytes[index + 1]);
            out.push(bytes[index + 2]);
            out.push('.');
            index += 4;
            continue;
        }
        out.push(c);
        index += 1;
    }
    out
}
