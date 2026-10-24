//! Interface language: the string catalog's one switch (ADR
//! `ui-strings-live-in-a-compile-time-catalog`).
//!
//! The catalog is `crates/ui/locales/<locale>/<group>.yml` — one file per
//! migration group per language, rust-i18n `_version: 2` YAML, discovered and
//! embedded at compile time by the `rust_i18n::i18n!` invocation at the crate
//! root (`lib.rs`); `t!` expands to `crate::_rust_i18n_t`, so that call can
//! never move in here. `build.rs` re-runs the build when the catalog changes.
//! Render sites use `use crate::i18n::t;` then `t!("group.key")`; the key's
//! first segment names the owning group's file.
//!
//! The active locale is a process-global inside rust-i18n, outside gpui's
//! change tracking — the same posture as [`crate::appearance`]'s imperative
//! `Theme::of(cx)` reads: no view knows its strings went stale. Switching
//! therefore goes through [`set_language`], which persists the choice, swaps
//! the global, marks every window dirty via `App::refresh_windows`, and
//! rebuilds the native menus (`cx.set_menus` snapshots titles at set time —
//! see `app_menus`). Build-time snapshots re-translate on their next
//! construction: already-posted notification chips, open menu cards, the
//! composer's failure notice, per-page entity state holding placeholder
//! text (a settings page built with `ComposerInput::new(t!(…))` keeps the
//! old language until re-entered), error strings cached inside
//! `Loadable::Error`/`save_error`/`images.rs`'s process-wide `ImageCache`
//! (until the next load retries), and staged-failure snapshots in `send`.

use std::borrow::Cow;

use gpui::{App, Global};

use crate::settings::{self, SavePolicy};

/// The translation macro, re-exported so call sites write
/// `use crate::i18n::t;` … `t!("group.key")`.
pub use rust_i18n::t;

/// The fallback locale: `en-US` values are the source strings, and the
/// crate-root `i18n!` names this locale as the compile-time fallback.
pub const DEFAULT_LOCALE: &str = "en-US";

/// One pickable language. `name` is the autonym — each language is always
/// shown in its own spelling (a locale-dependent autonym would make the
/// picker unfindable in the wrong language), so it is a constant, not a
/// catalog key.
pub struct Language {
    pub code: &'static str,
    pub name: &'static str,
}

/// The languages the catalog ships, in picker order.
pub const LANGUAGES: [Language; 2] = [
    Language {
        code: "en-US",
        name: "English",
    },
    Language {
        code: "zh-CN",
        name: "简体中文",
    },
];

/// The language state installed at boot: what the user asked for, and the
/// locale that actually took effect. [`set_language`] is the only writer
/// after init.
pub struct I18nState {
    /// The persisted preference (`ui-settings.json` `interfaceLanguage`).
    /// `None` follows the OS, resolved once per launch and never written
    /// back — the same policy as `appearance`. A stored code outside
    /// [`LANGUAGES`] is treated as unset rather than honored.
    pub preference: Option<String>,
    /// The locale the catalog is currently serving.
    pub locale: String,
}

impl Global for I18nState {}

/// The locale in effect (`DEFAULT_LOCALE` before [`init`]).
pub fn locale(cx: &App) -> &str {
    cx.try_global::<I18nState>()
        .map(|state| state.locale.as_str())
        .unwrap_or(DEFAULT_LOCALE)
}

/// Resolve a preference against the OS locale: an explicit supported choice
/// always wins; otherwise the OS language maps onto the catalog (`zh*` →
/// `zh-CN` while it is the only Chinese variant shipped), and anything the
/// catalog does not carry — or a failed probe — falls back to English.
/// Pure, so the mapping is directly testable.
pub fn resolve(preference: Option<&str>, os_locale: Option<&str>) -> Cow<'static, str> {
    if let Some(language) = preference.and_then(supported) {
        return Cow::Borrowed(language.code);
    }
    Cow::Borrowed(
        os_locale
            .and_then(match_os_locale)
            .unwrap_or(DEFAULT_LOCALE),
    )
}

/// The [`Language`] a supported code names. A code the catalog does not ship
/// logs once and resolves as unset — a hand-edited `interfaceLanguage` must
/// not wedge the UI in an untranslated locale.
fn supported(code: &str) -> Option<&'static Language> {
    LANGUAGES
        .iter()
        .find(|language| language.code == code)
        .or_else(|| {
            tracing::warn!(
                code,
                "stored interface language is not shipped; following the OS"
            );
            None
        })
}

/// Map an OS locale onto the catalog. Only the primary language subtag
/// matters; `zh-Hans-CN` and `zh_TW` are both Chinese to this table.
fn match_os_locale(os_locale: &str) -> Option<&'static str> {
    let primary = os_locale
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match primary.as_str() {
        "zh" => Some("zh-CN"),
        "en" => Some(DEFAULT_LOCALE),
        _ => None,
    }
}

/// Apply the boot locale. Call after `settings::init` (the preference is in
/// hand) and before any menu or view is built, so the first native menu and
/// the first frame are already localized. Never writes the settings file:
/// a follow-the-OS user keeps following the OS across launches.
pub fn init(preference: Option<String>, cx: &mut App) {
    let locale = resolve(preference.as_deref(), sys_locale::get_locale().as_deref()).into_owned();
    rust_i18n::set_locale(&locale);
    cx.set_global(I18nState {
        preference: preference.filter(|code| supported(code).is_some()),
        locale,
    });
}

/// Apply a language choice: `None` returns to following the OS. Persists the
/// preference through the settings store (durable choices flush immediately,
/// like `appearance::set_mode`), swaps the catalog locale, and repaints —
/// `refresh_windows`, not per-entity notify, because `t!` is an imperative
/// read no view tracks — then rebuilds the native menu bar, whose titles are
/// a snapshot from `set_menus` time.
pub fn set_language(preference: Option<&str>, cx: &mut App) {
    let preference = preference
        .filter(|code| supported(code).is_some())
        .map(str::to_owned);
    let locale = resolve(preference.as_deref(), sys_locale::get_locale().as_deref()).into_owned();
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.interface_language = preference.clone();
    });
    let unchanged = cx
        .try_global::<I18nState>()
        .is_some_and(|state| state.preference == preference && state.locale == locale);
    if unchanged {
        return;
    }
    rust_i18n::set_locale(&locale);
    cx.set_global(I18nState { preference, locale });
    cx.refresh_windows();
    cx.set_menus(crate::app_menus::app_menus());
}

/// Translate by a runtime-computed key. The catalog completeness tests scan
/// for `t!`'s literal first argument, which a dynamic key hides — every
/// `ts` call site must also spell its possible keys as literal `t!` calls
/// (usually in its own test) or the two-language key-set check cannot see
/// them. Prefer [`t`] with a literal. The result borrows `key` because a
/// missing key renders as the key itself.
pub fn ts(key: &str) -> Cow<'_, str> {
    t!(key)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::prelude::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// rust-i18n's active locale is process-global and tests run in parallel
    /// threads of one process: any test that asserts on catalog strings and
    /// any test that moves the locale must hold this lock, or a switching
    /// test can flip the language under a concurrently asserting one.
    ///
    /// Holding the guard also means "the locale is `en` right now": every
    /// locked test starts from the default, and the guard resets it on drop
    /// — even when the test panics mid-switch — so one failure cannot
    /// cascade into every later English assertion.
    pub(crate) fn locale_lock() -> LocaleGuard {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_locale();
        LocaleGuard(guard)
    }

    /// The locale lock; resets the process locale to the default on drop
    /// (the field's `MutexGuard` is released after this body runs, so the
    /// reset happens while the lock is still held).
    pub(crate) struct LocaleGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl Drop for LocaleGuard {
        fn drop(&mut self) {
            reset_locale();
        }
    }

    /// Reset the process locale to rust-i18n's default ("en", resolved onto
    /// `en-US` by the fallback). The guard does this automatically; explicit
    /// calls remain harmless.
    pub(crate) fn reset_locale() {
        rust_i18n::set_locale("en");
    }

    #[test]
    fn an_explicit_choice_always_wins() {
        for os in [Some("zh-CN"), Some("fr-FR"), None] {
            assert_eq!(resolve(Some("en-US"), os), "en-US");
            assert_eq!(resolve(Some("zh-CN"), os), "zh-CN");
        }
    }

    #[test]
    fn none_follows_the_os_primary_language_only() {
        assert_eq!(resolve(None, Some("zh-Hans-CN")), "zh-CN");
        assert_eq!(resolve(None, Some("zh_TW")), "zh-CN");
        assert_eq!(resolve(None, Some("en-GB")), "en-US");
        // Anything the catalog does not carry, or a failed probe, is English.
        assert_eq!(resolve(None, Some("fr-FR")), "en-US");
        assert_eq!(resolve(None, None), "en-US");
    }

    #[test]
    fn an_unsupported_stored_choice_behaves_as_unset() {
        assert_eq!(resolve(Some("fr"), Some("zh-CN")), "zh-CN");
        assert_eq!(resolve(Some("fr"), None), "en-US");
    }

    #[test]
    fn language_names_are_autonyms() {
        assert_eq!(LANGUAGES[0].name, "English");
        assert_eq!(LANGUAGES[1].name, "简体中文");
    }

    // ---- the catalog itself ----------------------------------------------
    //
    // The catalog files are parsed by a purpose-built subset parser (below).
    // The runtime path is rust-i18n's, so `catalog_resolves_through_rust_i18n`
    // asserts every parsed value back through `t!` to catch any divergence
    // between the two readers.

    /// dotted key → language tag → value, across every non-empty group file.
    type Catalog = std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

    fn read_catalog() -> Catalog {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let mut catalog: Catalog = Default::default();
        for locale_dir in ["en-US", "zh-CN"] {
            let mut paths: Vec<_> = std::fs::read_dir(root.join(locale_dir))
                .unwrap_or_else(|error| panic!("locales/{locale_dir}: {error}"))
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "yml"))
                .collect();
            paths.sort();
            for path in paths {
                let text = std::fs::read_to_string(&path).unwrap();
                if text.trim().is_empty() {
                    continue;
                }
                let group = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                for (key, tag, value) in parse_group_yml(&text)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
                {
                    assert_eq!(
                        key.split('.').next(),
                        Some(group.replace('-', "_").as_str()),
                        "{}: key {key} starts outside the file's own first segment",
                        path.display()
                    );
                    // The leaf tag must name the directory exactly — never a
                    // bare `en`/`zh`, whose sub-tag fallback would mask a
                    // mistagged entry at runtime.
                    assert_eq!(
                        tag,
                        locale_dir,
                        "{}: key {key} is tagged {tag}; the tag must name the directory",
                        path.display()
                    );
                    let tags = catalog.entry(key.clone()).or_default();
                    assert!(
                        tags.insert(tag.clone(), value.clone()).is_none(),
                        "{}: key {key} is defined twice for {tag} — the later \
                         line would silently win",
                        path.display()
                    );
                }
            }
        }
        catalog
    }

    /// Parse one group file: a `_version: 2` first line, then 2-space
    /// nesting, each leaf a single `  <locale-tag>: value` line. This subset
    /// IS the file convention — anything richer (block scalars, anchors,
    /// flow collections) fails here instead of diverging at runtime. Keys
    /// and values that YAML 1.1 would resolve to a boolean/null/number must
    /// be double-quoted (`"off":`, `en-US: "On"`) — the bare forms silently
    /// diverge between this parser and rust-i18n's runtime (the lost
    /// `web_search.off` key was this class). Values containing `: ` or
    /// starting with `%` must be quoted likewise; a `key:` line with no
    /// value opens a nested node, so a genuinely empty string value is
    /// written `""`.
    fn parse_group_yml(text: &str) -> Result<Vec<(String, String, String)>, String> {
        let mut lines = text.lines().enumerate();
        match lines.next() {
            Some((_, first)) if first.trim_end() == "_version: 2" => {}
            _ => return Err("first line must be exactly `_version: 2`".into()),
        }
        let mut stack: Vec<String> = Vec::new();
        let mut depths: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        for (number, line) in lines {
            let trimmed = line.trim_end();
            if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            if indent % 2 != 0 {
                return Err(format!("line {}: indent must be 2-space steps", number + 1));
            }
            while depths.last().is_some_and(|open| *open >= indent) {
                depths.pop();
                stack.pop();
            }
            // Each nesting level adds one 2-space step, so the surviving
            // ancestor chain must be exactly half the indent — this rejects
            // a level skipped or collapsed.
            if depths.len() != indent / 2 {
                return Err(format!(
                    "line {}: indent jumps a level ({} ancestors open, {} spaces)",
                    number + 1,
                    depths.len(),
                    indent
                ));
            }
            let (key, value) = trimmed
                .trim_start()
                .split_once(": ")
                .or_else(|| {
                    trimmed
                        .trim_start()
                        .ends_with(':')
                        .then(|| (&trimmed.trim_start()[..trimmed.trim_start().len() - 1], ""))
                })
                .ok_or_else(|| format!("line {}: expected `key: value`", number + 1))?;
            // Keys may be double-quoted — the sanctioned spelling for a
            // segment that would otherwise resolve to a non-string
            // (`"off":`), kept in step with the runtime parser's quoting.
            let key_quoted = key.len() >= 2 && key.starts_with('"') && key.ends_with('"');
            let key = if key_quoted {
                &key[1..key.len() - 1]
            } else {
                key
            };
            if key.contains(' ') {
                return Err(format!("line {}: keys cannot contain spaces", number + 1));
            }
            if !key_quoted && let Some(kind) = yaml_scalar_word(key) {
                return Err(format!(
                    "line {}: key {key:?} resolves to {kind} in YAML 1.1, so rust-i18n \
                     drops it — quote it (`\"{key}\":`) or rename the segment",
                    number + 1
                ));
            }
            if value.is_empty() {
                // A node-opening line (`key:`), possibly without a space.
                stack.push(key.to_string());
                depths.push(indent);
            } else {
                // A leaf: the key is the language tag, the value the string.
                if stack.is_empty() {
                    return Err(format!(
                        "line {}: a leaf must sit under at least the group's first-segment key",
                        number + 1
                    ));
                }
                let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
                    &value[1..value.len() - 1]
                } else if let Some(kind) = yaml_scalar_word(value) {
                    return Err(format!(
                        "line {}: value {value:?} resolves to {kind} in YAML 1.1, so \
                         rust-i18n drops the entry — double-quote it (\"{value}\")",
                        number + 1
                    ));
                } else if value.starts_with('%') || value.contains(": ") {
                    return Err(format!(
                        "line {}: value {value:?} must be double-quoted",
                        number + 1
                    ));
                } else {
                    value
                };
                out.push((stack.join("."), key.to_string(), value.to_string()));
            }
        }
        Ok(out)
    }

    /// The plain scalars YAML 1.1 resolves to a non-string, which is where
    /// the subset parser (reads a string) and rust-i18n's runtime (drops or
    /// retypes it) diverge. `Some(kind)` names the resolution for the error
    /// message; the caller decides whether quoting or renaming is right.
    fn yaml_scalar_word(word: &str) -> Option<&'static str> {
        match word.to_ascii_lowercase().as_str() {
            "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" => Some("a boolean"),
            "null" | "~" => Some("null"),
            _ => {
                if word.parse::<i64>().is_ok() || word.parse::<f64>().is_ok() {
                    Some("a number")
                } else {
                    None
                }
            }
        }
    }

    /// The subset parser's own contract, on samples — the shipped files
    /// only exercise the happy path.
    #[test]
    fn yaml_scalar_words_are_rejected_unless_quoted() {
        // A bare boolean-like key segment: the class that cost the
        // `web_search.off` key (rust-i18n dropped it, this parser kept it).
        let err = parse_group_yml("_version: 2\ng:\n  off:\n    en-US: Off\n")
            .expect_err("bare `off` segment must be rejected");
        assert!(err.contains("resolves to a boolean"), "{err}");
        // The same word quoted is a plain string segment again.
        let ok = parse_group_yml("_version: 2\ng:\n  \"off\":\n    en-US: \"Off\"\n")
            .expect("quoted key and value parse");
        assert_eq!(
            ok[0],
            ("g.off".to_string(), "en-US".to_string(), "Off".into())
        );
        // A bare boolean-like or numeric whole value would be dropped or
        // retyped at runtime — rejected here with the quoting hint.
        for value in ["On", "no", "100", "1.5", "null"] {
            let err = parse_group_yml(&format!("_version: 2\ng:\n  k:\n    en-US: {value}\n"))
                .expect_err("bare scalar value must be rejected");
            assert!(err.contains("double-quote"), "{value}: {err}");
        }
        // Ordinary words and strings-with-colons (quoted) stay fine.
        assert!(parse_group_yml("_version: 2\ng:\n  k:\n    en-US: Done\n").is_ok());
        assert!(parse_group_yml("_version: 2\ng:\n  k:\n    en-US: \"On demand\"\n").is_ok());
    }

    #[test]
    fn catalog_files_parse_first_segments_and_tags_are_well_formed() {
        let catalog = read_catalog();
        assert!(
            !catalog.is_empty(),
            "the seeded keys (app menus, language row, notifications-status) should be present"
        );
        for (key, tags) in &catalog {
            // Exactly the two shipped languages tag each key, with the
            // directory-named tag — never a bare `en`/`zh`, whose sub-tag
            // fallback would mask a mistagged entry.
            let mut tagged: Vec<_> = tags.keys().collect();
            tagged.sort_unstable();
            assert_eq!(
                tagged,
                ["en-US", "zh-CN"],
                "key {key} must be tagged exactly en-US and zh-CN"
            );
        }
    }

    #[test]
    fn every_key_exists_in_both_languages() {
        let catalog = read_catalog();
        let mut missing = Vec::new();
        for (key, tags) in &catalog {
            for tag in ["en-US", "zh-CN"] {
                if !tags.contains_key(tag) {
                    missing.push(format!("{key} ({tag})"));
                }
            }
        }
        assert!(missing.is_empty(), "missing from the catalog: {missing:?}");
    }

    /// The `{name}` tokens an interpolated value carries, as a sorted
    /// multiset: order may differ across languages, names and counts may
    /// not — a translation that drops or renames one renders without its
    /// variable at runtime, and nothing else catches it.
    fn placeholder_names(value: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut chars = value.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '{' {
                continue;
            }
            let mut name = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                match c {
                    '}' if !name.is_empty() => {
                        closed = true;
                        break;
                    }
                    c if c.is_ascii_alphanumeric() || c == '_' => name.push(c),
                    _ => break,
                }
            }
            if closed {
                names.push(name);
            } else if !name.is_empty() {
                // `{` followed by a name that never closes — surface it as
                // its own token so the mismatch names the broken value.
                names.push(format!("UNCLOSED_{name}"));
            }
        }
        names.sort();
        names
    }

    #[test]
    fn placeholder_names_match_across_languages() {
        let catalog = read_catalog();
        let mut mismatches = Vec::new();
        for (key, tags) in &catalog {
            let Some(en) = tags.get("en-US") else {
                continue;
            };
            for (tag, value) in tags {
                if tag != "en-US" && placeholder_names(en) != placeholder_names(value) {
                    mismatches.push(format!(
                        "{key} ({tag}): en-US {:?} vs {tag} {:?}",
                        placeholder_names(en),
                        placeholder_names(value)
                    ));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "interpolation tokens diverge: {}",
            mismatches.join("; ")
        );
    }

    #[test]
    fn every_literal_t_key_exists_in_the_catalog() {
        // The scan technique of docs/research/gpui-i18n.md §9.6: extract the
        // literal first argument of every `t!("…")` under src/ (word
        // boundary — `format!(` and `assert!(` also end in `t!(`) and
        // require each to be defined in both languages. No allowlist:
        // non-literal lookups go through `ts`, which documents the
        // literal-`t!` requirement. Comment lines are skipped — a doc
        // mention must not register as a call. Two spellings of the same
        // literal are normalized: rustfmt breaks a long call as `t!(` +
        // newline + `"key"`, and a hand-written `\`+newline continuation
        // inside the literal compiles to the joined key.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut used: std::collections::BTreeSet<String> = Default::default();
        // Spelled in two pieces so this file's own source never contains
        // the needle followed by a quote (the scan would read itself).
        let needle = concat!("t", "!(");
        for file in walk_rs_files(&src) {
            let text = std::fs::read_to_string(&file).unwrap();
            let lf_text = text.replace("\r\n", "\n");
            let crlf_text = lf_text.replace('\n', "\r\n");
            for text in [lf_text.as_str(), crlf_text.as_str()] {
                let mut at = 0;
                while let Some(hit) = text[at..].find(needle) {
                    let start = at + hit;
                    at = start + 3;
                    // The byte before `t` must not be a name character.
                    let boundary = text[..start]
                        .chars()
                        .next_back()
                        .map(|prev| !prev.is_ascii_alphanumeric() && prev != '_')
                        .unwrap_or(true);
                    // Skip whole-line comments: find the line start and see if
                    // anything but `//` precedes the hit.
                    let line_start = text[..start].rfind('\n').map(|at| at + 1).unwrap_or(0);
                    let comment = text[line_start..start].trim_start().starts_with("//");
                    if !boundary || comment {
                        continue;
                    }
                    // rustfmt may put the literal on its own line after `t!(`.
                    let after_paren = text[at..].trim_start();
                    let Some(body) = after_paren.strip_prefix('"') else {
                        continue; // a non-literal first argument (`ts` path)
                    };
                    let mut key = String::new();
                    let mut chars = body.chars().peekable();
                    let mut closed = false;
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => {
                                closed = true;
                                break;
                            }
                            // `\` + LF/CRLF: a string continuation — the
                            // compiler drops the line ending and the next line's
                            // leading whitespace, so the scan does the same.
                            '\\' if matches!(chars.peek(), Some(&'\n') | Some(&'\r')) => {
                                if chars.next() == Some('\r') && chars.peek() == Some(&'\n') {
                                    chars.next();
                                }
                                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                                    chars.next();
                                }
                            }
                            '\\' => {
                                if let Some(escaped) = chars.next() {
                                    key.push(escaped);
                                }
                            }
                            other => key.push(other),
                        }
                    }
                    if closed {
                        used.insert(key);
                    }
                }
            }
        }
        assert!(!used.is_empty(), "the scan should see migrated call sites");
        let catalog = read_catalog();
        let mut missing: Vec<String> = used
            .iter()
            .filter(|key| {
                !catalog
                    .get(*key)
                    .is_some_and(|tags| tags.contains_key("en-US") && tags.contains_key("zh-CN"))
            })
            .cloned()
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "t! keys with no entry in locales/ (both languages required): {missing:?}"
        );
    }

    fn walk_rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
        out
    }

    /// The runtime spot check: every value the subset parser read must come
    /// back from rust-i18n itself, in both locales — this catches a
    /// subset/runtime YAML divergence (e.g. an unquoted `: ` silently
    /// nesting instead of yielding a string).
    #[test]
    fn catalog_resolves_through_rust_i18n() {
        let _guard = locale_lock();
        for (key, tags) in read_catalog() {
            for tag in ["en-US", "zh-CN"] {
                let Some(expected) = tags.get(tag) else {
                    continue;
                };
                assert_eq!(
                    t!(key.as_str(), locale = tag).as_ref(),
                    expected.as_str(),
                    "rust-i18n and the subset parser disagree on {key} ({tag})"
                );
            }
        }
        reset_locale();
    }

    /// Missing keys render as the key itself — the visible-not-crashing
    /// floor the migration relies on while surfaces move over group by
    /// group. Looked up through `ts` (the dynamic path) so the literal
    /// scanner does not register a key the catalog deliberately omits.
    #[test]
    fn a_missing_key_renders_as_the_key() {
        let _guard = locale_lock();
        assert_eq!(ts("no.such.key").as_ref(), "no.such.key");
        reset_locale();
    }

    /// A real view re-renders with the new locale after [`set_language`]'s
    /// `refresh_windows` — the repaint path is where a process-global locale
    /// silently fails (docs/research/gpui-i18n.md §9.6).
    #[gpui::test]
    fn switching_language_re_renders_a_live_view(cx: &mut gpui::TestAppContext) {
        let _guard = locale_lock();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
            // Pin an explicit choice: `None` would resolve through this
            // machine's real OS locale.
            set_language(Some("en-US"), cx);
        });
        struct Probe {
            rendered: std::sync::Arc<Mutex<String>>,
        }
        impl gpui::Render for Probe {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let label = t!("notifications_status.todo.todo").to_string();
                *self.rendered.lock().unwrap() = label.clone();
                gpui::div().id("probe").child(label)
            }
        }
        let rendered = std::sync::Arc::new(Mutex::new(String::new()));
        let (probe, visual) = cx.add_window_view(|_, _| Probe {
            rendered: rendered.clone(),
        });
        // Force the first frame: a window only paints after a notify.
        probe.update(&mut *visual, |_, cx| cx.notify());
        visual.run_until_parked();
        assert_eq!(rendered.lock().unwrap().as_str(), "Todo");

        // The switch must both move the catalog and repaint the live view.
        visual.update(|_, cx| set_language(Some("zh-CN"), cx));
        visual.run_until_parked();
        assert_eq!(rendered.lock().unwrap().as_str(), "待办");

        let persisted = visual.read(settings::current);
        assert_eq!(persisted.interface_language.as_deref(), Some("zh-CN"));
        reset_locale();
    }
}
