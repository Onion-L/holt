# UI Localization in GPUI Applications: Zed, gpui-component, and a Route for Holt

> Survey date: 2026-10-24. This document surveys how UI localization (i18n) is done — and not done — in the two relevant GPUI projects, establishes that the vendored GPUI snapshot provides no localization primitives, and recommends a concrete route for Holt. It is written for engineers already familiar with Rust; the GPUI mental model itself is covered by [gpui.md](gpui.md). Localization here means translated interface strings, not input methods or locale-aware data formatting, which are separate concerns.

## 1. Conclusions up front

- **Vendored GPUI ships zero i18n primitives.** No message catalog, no locale type, no lookup, no fallback, no plural rules. `SharedString` — the currency every label, menu item, and placeholder accepts — is a plain immutable `SmolStr` wrapper with no locale awareness. Any localization layer must be built entirely on the app side, without touching `third_party/gpui`. [H2][H3]
- **Zed, the largest GPUI application, has no localization layer at all.** Zero `.ftl` files, zero fluent/gettext/i18n dependencies (the only "fluent" package in `Cargo.lock` is `fluent-uri`, a URI parser), and UI text is hardcoded English literals from menu titles down to empty-state buttons. The only locale-aware code is `crates/time_format`, which sniffs the OS locale once for 12/24-hour and date-field order. There is no runtime language switching and no fallback mechanism. Official i18n is an open tracking issue (#7409, since 2024-02) with no plan or owner; an unadopted community RFC (#43592) records the decision points. [Z1][Z2][Z3][U1][U2]
- **gpui-component is the only GPUI-ecosystem project with working i18n**, built on `rust-i18n` 4.2: one YAML file (`locales/ui.yml`, `_version: 2`) per crate holding all languages, compiled into the binary by the `i18n!` macro; `t!("Dotted.Key")` at render sites; a process-global locale that sits outside GPUI's change tracking, so switching requires manual `cx.notify()` / `cx.refresh_windows()` plus explicit rebuild of cached menus. Fallback is three layers: explicit `en`, BCP-47 subtag lookup, then the key string itself. [G1][G2][G4][G5]
- **Recommended for Holt:** adopt `rust-i18n` with a holt-owned `crates/ui/locales/ui.yml` in the same format; `t!()` at render sites with keys namespaced by surface; interface language persisted as a new device-local `UiSettings` key (`Option<String>`; absent/`None` = follow the OS language, resolved once per launch via `sys-locale` against the supported set, else `en`); locale applied during bootstrap right after `settings::init`; hot switch via `settings::update` → `set_locale` → `cx.refresh_windows()` → native menu rebuild. Rejected: the Fluent stack, gettext, a homegrown catalog, waiting for Zed upstream, and an engine-owned locale RPC. (§8)
- **The cost of waiting scales with string count.** Re-counted on 2026-10-24, `crates/ui` carries 1,929 capitalized English string literals — 1,248 unique, which is the realistic catalog scale — across 141 files: small enough to migrate surface-by-surface. Zed's years of accumulated literals are precisely what blocks its own RFC; the cheapest moment to add the translation seam is now. [H4]

## 2. Source snapshots and method

| Snapshot | Pin | Role here |
| --- | --- | --- |
| Zed clone (`/tmp/zed-research`) | `9f305aaf` (2026-10-23) | The largest GPUI app; the no-i18n case study |
| gpui-component clone (`/tmp/gpui-component-research`) | `ecd2c8f` (2026-10-24) | The only ecosystem i18n implementation |
| `third_party/gpui` | frozen vendored snapshot | What the framework itself provides |
| Holt `crates/ui` | `main` @ `8a1bf10` | Migration target |

During this survey the clones' HEADs were confirmed to match the pins above, the key holt/ui file locations were re-read, and the following were re-run on 2026-10-24: the vendored-gpui greps, the string-literal counts, the workspace dependency greps (no gpui-component/gpui-kit, no `sys-locale`), and the `t!`-key source scan against the gpui-component clone (§9.6). Clone-side line references below were spot-verified against the pinned commits; module-level count breakdowns in §7.1 come from the survey of the same match set. Zed-internal paths are cited as `crates/...` under the Zed pin; gpui-component paths likewise.

## 3. Zed: the largest GPUI app has no localization layer

### 3.1 Dependencies and assets: none

At HEAD `9f305aaf`, `git ls-tree -r --name-only HEAD | grep -c '\.ftl$'` returns 0, and a tree-wide grep for `locales?/|i18n|l10n` returns nothing. `Cargo.lock` contains no fluent/fluent-syntax/fluent-bundle/fluent-langneg/fluent-templates/i18n-embed/unic-langid/gettext package. The only "fluent" package is `fluent-uri` (Cargo.lock:6463), a URI parser unrelated to Mozilla Fluent. The `icu_*` crates are transitive Unicode normalization/IDNA dependencies, not message formatting. [Z1]

The sole system-locale dependency is `sys-locale` 0.3.1 (root `Cargo.toml:920`), and `git grep -l 'sys-locale' HEAD -- '*/Cargo.toml'` shows exactly one consumer: `crates/time_format`. [Z1]

### 3.2 Strings are literals at every level

There is no translation abstraction — no trait, macro, or type between code and the English string. Verified examples:

- `Button::new("open_project", "Open Project")` — the first argument is the element id, the second is the hardcoded label (`crates/ui/src/components/project_empty_state.rs:71`). [Z2]
- `MenuItem::action("About Zed", zed_actions::About)` / `MenuItem::action("Check for Updates", auto_update::Check)` — native menu titles (`crates/zed/src/zed/app_menus.rs:74-75`). [Z2]
- Hand-rolled English plurals: `"1 minute ago"` / `format!("{} minutes ago", minutes)` and `if years == 1 { "year" } else { "years" }`, including a composed `"1 year, 10 months ago"` format (`crates/time_format/src/time_format.rs:263-321`) — exactly the problem class CLDR plural rules exist to solve. [Z3]

### 3.3 The only locale-aware code is time formatting

`crates/time_format` is the entire extent of Zed's locale awareness, and none of it touches UI copy:

- Linux: `sys_locale::get_locale()` read once into `static CURRENT_LOCALE: OnceLock<String>` (`time_format.rs:469`; `get_or_init` at `:108-109`), with a literal `"en-US"` fallback — the only "fallback locale" in the project. Verified by re-reading. [Z3]
- `is_12_hour_time_by_locale` uses a hardcoded locale whitelist; anything unlisted is treated as 24-hour (`time_format.rs:482-496`). [Z3]
- macOS uses `CFLocaleCopyCurrent` + `CFDateFormatter` (`:498-573`); Windows uses WinRT `DateTimeFormatter` (`:575-624`) — OS formatters for date/time rendering only. [Z3]

### 3.4 No runtime switching, no fallback, no setting

There is no language state, no re-render path, and no persisted setting. Greps for `ui_language|interface_language|app_language|locale` in the settings crates return nothing; `crates/settings_content/src/language.rs` is per-programming-language editor configuration (tab size and the like), unrelated to UI language. UI copy never varies with the system language. [Z1]

### 3.5 The unadopted RFC and what it settled

Community RFC discussion [#43592](https://github.com/zed-industries/zed/discussions/43592) (2025-11-26, proposer states "I'm not a Zed dev") is unadopted but records the decision points that any self-built layer will face: [U1]

- **Do not embed translations in the binary** — "embedding is a non-starter", otherwise translators must build Zed to see their work. (Holt's calculus differs: it ships a small fixed language set with the app, so compile-time embedding is acceptable — see §8.2.)
- **Key strategy**: a `t!(key)` macro, locale-aware components, or hashing English source strings as keys.
- **Late binding** (advocated by Fluent's author zbraniecki): UI elements bind a localization ID so language can switch at runtime without app-level refreshes.
- Suggested tooling: `fluent-langneg-rs` for language negotiation, Pontoon as the translation platform, a possible far path to ICU4X/MessageFormat 2.
- OS-native surfaces (app menu, system notifications) explicitly out of scope for a first pass — a layer gpui does not draw.

Tracking issue [#7409](https://github.com/zed-industries/zed/issues/7409) has been open since 2024-02 with the `area:internationalization` label and no plan, timeline, or team commitment. [U2]

### 3.6 The zh-CN path is community forks, not the product

Official zh-CN interface requests (discussions [#56838](https://github.com/zed-industries/zed/discussions/56838), [#64167](https://github.com/zed-industries/zed/discussions/64167)) have no official response. Working Chinese interfaces exist only as community forks (x6nux/zed-globalization, TC999/zed-loc, LI-NA/zed-i18n, axxion/Zed-L10n) that inject translations at build time and ship patched binaries; none can enter the official build as an extension. [U3]

### 3.7 Grep traps when surveying for Fluent in this ecosystem

`grep fluent` in any gpui codebase is full of false positives: `FluentBuilder` is gpui's chainable-builder trait (`crates/gpui/src/element.rs:159` in Zed; `gpui_util/src/lib.rs:1` in Holt's vendored copy — re-verified), and `fluent-uri` is a URI parser. Neither has anything to do with Mozilla Fluent. [Z1][H3]

## 4. gpui-component: rust-i18n inside a component library

### 4.1 Dependency and asset format

The only i18n dependency is `rust-i18n` 4.2 (workspace declaration `rust-i18n = "4.2.0"`, resolves to 4.2.1; consumers: `crates/component/Cargo.toml:134` and `crates/story/Cargo.toml:37` — both re-verified). No fluent crates, no `sys-locale`. [G1]

Each participating crate carries one `locales/ui.yml` in rust-i18n's `_version: 2` YAML format: a single file holding **all** languages, nested keys mapping to per-language values — [G2]

```yaml
_version: 2
Attachment:
  Remove:
    en: Remove
    pt-BR: "Remover"
    zh-CN: 移除
```

(re-verified at `crates/component/locales/ui.yml:1-14`). There are no per-language directories and no `.ftl` files. Coverage of the built-in catalog: en / zh-CN / zh-HK / pt-BR ≈ 120 keys each, zh-TW 83, it 44; the docs state plainly "Coverage varies by key". [G2][G3]

Translations are embedded into the binary **at compile time** by the `i18n!` macro (`rust_i18n::i18n!("locales", fallback = "en")` at `crates/component/src/lib.rs:125`, re-verified). There is no runtime resource IO; adding a language or key requires a rebuild. [G2]

### 4.2 The string API

There is no `Language` enum, no `LanguageService`, no `Localize` trait — the locale is a plain string (the only `Language` types in the repo are syntax-highlighting ones). The pattern at render sites: [G4]

```rust
use rust_i18n::t;
// ...
1 => t!("Calendar.month.January")
```

`t!` takes a dotted nested key and returns `Cow<str>`, which `.into()`s directly into `SharedString` — the whole integration with GPUI is producing a string where a literal used to be. Verified call sites: month names (`crates/component/src/time/calendar.rs:6,15-19`), pagination (`pagination.rs:7,119-121`), and placeholder defaults via `.unwrap_or_else(|| t!("Combobox.placeholder").into())` (`combobox.rs:553`; same shape in `select.rs:483/523/1061`). No parameterized `t!` calls exist in the component crate. The crate re-exports thin `locale() -> impl Deref<Target = str>` / `set_locale(&str)` wrappers (`lib.rs:153-160`, re-verified) so applications can switch without depending on rust-i18n directly. [G4]

### 4.3 Runtime switching and repaint

The active locale is a **process-global static outside GPUI's change tracking** (the docs' own words, `website/docs/i18n.md:118`, re-verified). Consequences, all documented and all load-bearing for Holt:

- `set_locale` alone schedules **no repaint**. A view-scoped picker does `set_locale(...); cx.notify();`; an app-wide action does `set_locale(language); cx.refresh_windows();` (`i18n.md:118-138`). `refresh_windows` is a real API the library itself uses (e.g. `crates/component/src/theme/mod.rs:332`). [G5]
- Cached presentation must be rebuilt explicitly: the story app's `SelectLocale` handler calls `set_locale` and then rebuilds its native and in-window menus (`crates/story/src/app_menus.rs:14-21,36-49`); menu checked-state is not a live translation lookup. [G5]
- No persistence — neither library nor demo writes the locale to disk; "Your application owns its own copy, locale policy, and formatting" (`i18n.md:9`). [G3]
- No system-locale detection — `component::init()` sets no locale (`lib.rs:128-150`, re-verified: no locale code at all); the default is `"en"`. Holt deliberately departs from this default (§8.1, §8.3).
- The demo's own menu labels stay English — the language picker demonstrates component translations, not a fully localized app (`i18n.md:140`). Do not copy it as a localization exemplar. [G3]

### 4.4 Fallback chain and app overrides

Three layers, all verified in source: [G5]

1. **Explicit fallback locale** — `fallback = "en"` in both `i18n!` invocations; a language missing a key falls back to English.
2. **BCP-47 subtag fallback** — `zh-Hant-CN-x-private1` → `zh-Hant-CN` → `zh-Hant` → `zh` (RFC 4647 §3.4), implemented in `_rust_i18n_lookup_fallback` (`rust-i18n-macro-4.2.1/src/lib.rs:388-395`; macro source re-checked in the local cargo registry).
3. **Missing key** — `t!` returns the key string itself (`i18n.md:98,162`), visible in the UI rather than a crash.

Applications can override component strings: `rust_i18n::extend!(gpui_component)` registers an application backend consulted **before** the built-ins (NamespacedBackend, macro `lib.rs:373-377`). The direction is one-way — an application can override component keys, but its own `t!` cannot see the component's built-in translations, so every application key must exist in the application's own file (`i18n.md:89-99`). The namespace must equal the crate name with hyphens underscored, key spellings must match the built-in file exactly, `extend!` must run before `gpui_kit::init`, and a second `extend!` panics (macro `lib.rs:365-369` assert "has already been extended"). [G5]

**Relevance to Holt: reference only.** Holt does not depend on gpui-component or gpui-kit — `rg -l 'gpui-component|gpui_component|gpui-kit|gpui_kit' --glob 'Cargo.toml'` across the workspace matches nothing (re-run 2026-10-24) — so there are no component built-in strings to override, and the `extend!` layer is **not** part of the §8 route: Holt's single catalog owns every string it renders. The mechanism is recorded because it is the documented pattern should Holt ever adopt those components, and because its one-way lookup rule is the kind of detail that costs a day when discovered by surprise. [H3]

## 5. Comparison

One row per subject; the vendored framework is listed first because everything below it is built on top of it.

| Subject | i18n dependency | Message format & lookup | Runtime switch | Fallback | Persistence / OS-locale detection | Plurals · RTL |
| --- | --- | --- | --- | --- | --- | --- |
| **Vendored GPUI** (Holt snapshot) | none — `rg 'fluent\|gettext\|locale'` on `crates/gpui/Cargo.toml` exits 1 (re-run) | none; `SharedString` is a locale-free `SmolStr` wrapper that every copy-bearing API accepts | n/a | n/a | n/a | n/a — the text system does glyph fallback only |
| **Zed** @ `9f305aaf` | none (`fluent-uri` is a URI parser; `sys-locale` used by `time_format` alone) | hardcoded English literals at every call site; no abstraction | none | none (a literal `"en-US"` inside time formatting only) | none / `time_format` only (12/24h, date-field order) | hand-written English plurals · none |
| **gpui-component** @ `ecd2c8f` | `rust-i18n` 4.2, single crate | one `locales/ui.yml` per crate, `_version: 2` YAML, all languages in one file, compile-time embedded; `t!("Dotted.Key")` → `Cow<str>` → `SharedString` | `set_locale` on a process-global static + manual `cx.notify()` / `cx.refresh_windows()` + explicit menu rebuild | `en` → BCP-47 subtag stripping → key-as-string | none (policy left to the app) / none (default `"en"`) | none (app-owned formatting) · none (strings switch, layout never mirrors) |
| **Holt today** | none | 1,929 English literals across 141 files (re-counted) | none | n/a | `UiSettings` / `ui-settings.json` is the natural home (§7.2) / none | hand-written where needed · n/a |

## 6. What the vendored GPUI snapshot actually provides

Re-running `rg -il 'fluent|i18n|localiz|gettext|translation' third_party/gpui --glob '!target'` on 2026-10-24, every hit is a false friend: license boilerplate; the "Segoe Fluent Icons" font name (`crates/gpui_wgpu/src/cosmic_text_system.rs:290`); OS platform APIs (`localizedDescription`, localized font family names, menu key-equivalent localization); geometric `translation` in Taffy/scene/path code; and the `FluentBuilder` builder trait (`crates/gpui_util/src/lib.rs:1`, used at `crates/gpui/src/element.rs:37,159`). `rg -in 'fluent|gettext|locale' third_party/gpui/crates/gpui/Cargo.toml` exits 1 — no message-catalog dependency of any kind. [H3]

`SharedString` (`crates/gpui_shared_string/gpui_shared_string.rs:10-39`) is an immutable, cheap-to-clone `SmolStr` wrapper with no locale, lookup, or fallback semantics — but it is what every copy-bearing API accepts (`impl Into<SharedString>`). That is the integration seam: a holt-side i18n layer only has to produce the right localized string at render time, and GPUI's text system handles shaping, measuring, and glyph fallback from there. GPUI's only adjacent responsibility is the font/fallback layer, which is why §9.3 treats CJK font coverage as a first-class migration check rather than an afterthought.

Per [docs/CODE-STANDARDS.md](../CODE-STANDARDS.md), the vendored snapshot is changed only with an uncontroversial reason; localization has none — everything in §8 lives in `crates/ui`.

## 7. Holt today: surface inventory and persistence facts

### 7.1 Scale and hotspots

Re-counted on 2026-10-24 with `rg -o '"[A-Z][^"]*[a-z][^"]*"' crates/ui/src --glob '*.rs'` (capitalized literals containing lowercase — an approximation of English UI copy): **1,929 matches, 1,248 unique literals, of which 865 are multi-word phrases**, across 141 `.rs` files. The survey's module-level breakdown of the same match set: root files 795, `shell/` 301, `settings/` 225, `transcript/` 162, `composer/` 150, `pickers/` 129, `terminal/` 55, `files/` 47, `changes/` 46, `markdown/` 19. Heaviest files: `scheduled.rs` (103), `pickers/logic.rs` (67), `settings/usage.rs` (62), `shell/spaces.rs` (61), `chat_manager.rs` (57), `settings.rs` (53). Plan the catalog around the **1,248 unique literals**; the 865 multi-word subset is merely the share that reads as prose rather than single-word verbs, not a smaller workload. [H4]

Canonical shapes, verified in source:

- Section/field labels — `section_label(theme, impl Into<SharedString>)` (`crates/ui/src/settings/widgets.rs:113`).
- Menu rows — `MenuRow::item("New Session", ...)`, `MenuRow::item("Settings…", ...)` (`crates/ui/src/shell/menu_bar.rs:91-92`). [H5]
- Native menus — `MenuItem::action("About Holt", About)`, `MenuItem::action("Settings", ...)` (`crates/ui/src/app_menus.rs:169-177`). [H5]
- Tooltips — `.tooltip(text_tooltip("Open run"))` (`scheduled.rs:906`), `popover::TextTooltip` (`rewind.rs:188`, `badges.rs:97`).
- Placeholders — `ComposerTextElement::new(placeholder: impl Into<SharedString>)` (`composer/input.rs:446`).
- OS notifications — `"Chat completed"` / `"Chat failed"` (`notifications.rs:188-189`).
- Status/error strings — `pub(crate) const ENGINE_NOT_CONNECTED: &str = "Engine not connected"` (`state.rs:327`, re-verified as the single spelling site the composer's offline styling matches against).

**Identity-vs-label trap:** `crates/ui/src/theme.rs:77-83` — `Holt`/`Orange`/`Amber`/… are simultaneously the serialized theme identity and the display name. Localization must split these: keep the identity stable, localize only the label projection. [H5]

### 7.2 Where a language setting belongs

Holt has two persistence worlds. (a) `ui-settings.json`, read and written by the UI crate itself: `UiSettings::load` (`crates/ui/src/settings.rs:880`) defaults on any failure, `save` (`:903`) writes atomically via tmp+rename; the sole in-process writer is the `SettingsStore` gpui `Global` (`:67-80`), mutations go through `settings::update` with a 400 ms debounce (`SAVE_DEBOUNCE_MS`, `:55`) or `SavePolicy::Immediate`, and `settings::flush` runs on quit (`lib.rs:172`). (b) Engine-owned settings via RPC (`rpc/src/lib.rs:120-146`: title/goal/web-search/mcp), landed on disk by the engine. All re-verified. [H6]

Interface language is a device-local UI preference of exactly the same class as `appearance`, `ui_font_family`, and `ui_font_size` (which already live in `UiSettings`, `settings.rs:252-327`, container-level `#[serde(default, rename_all = "camelCase")]` — so an old file without the new key still loads, and the new key needs `#[serde(default)]` semantics). `appearance` even models the first-launch policy language needs: "Light/dark preference. Defaults to following the OS." (`settings.rs:321`) — OS-derived until the user chooses, persisted once they do. Although `ARCHITECTURE.md` says all disk writes are engine-owned, `ui-settings.json` is the long-standing sanctioned exception (it is not even listed in the data-on-disk inventory), and appearance/font/theme set the precedent: follow `UiSettings`, add no RPC. [H6][H0]

### 7.3 Bootstrap mounting point

`run_app` (`crates/ui/src/lib.rs:100`) sequences: `settings::UiSettings::load(&data_dir)` + `settings::init` at `lib.rs:125-126`, typography/theme/appearance init, then `app_menus::init` at `lib.rs:147`, then `AppState` bootstrap. The initial locale must be applied **after** `settings::init` (the setting is in hand) and **before** `app_menus::init` and the first render, so the first native menu build and first frame are already localized. The existing `SettingsStore` `Global` (`settings.rs:80`) is the structural precedent for app-wide state installed at this point. [H7]

## 8. Recommended route for Holt (DECISION.md material)

### 8.1 The choice

Adopt **`rust-i18n` 4.2** as the only i18n dependency, in `crates/ui`:

- One holt-owned catalog `crates/ui/locales/ui.yml`, rust-i18n `_version: 2` YAML, all languages in one file, keys namespaced by surface (§9.1), English authored inline as both source and fallback.
- `rust_i18n::i18n!("locales", fallback = "en")` at crate init; `t!()` at render sites producing strings that flow into `SharedString` unchanged.
- Interface language persisted as a new `UiSettings` key `interfaceLanguage: Option<String>` (device-local, camelCase, `#[serde(default)]` so an absent key deserializes to `None`). **First-launch policy: `None` means follow the OS language**, mirroring `appearance`, which "Defaults to following the OS" (`settings.rs:321`). At bootstrap — immediately after `settings::init` (`lib.rs:126`), before any menu or view is built — resolve `Some(code)` as-is; resolve `None` by reading the OS locale once via `sys-locale` (Zed's exact crate for the same job, root `Cargo.toml:920`) and matching it against the supported set: `zh*` → `zh-CN` while zh-CN is the only Chinese variant shipped (refine when zh-TW/zh-HK catalogs exist), anything else or a detection failure → `en`; then `set_locale`. An explicit choice in Settings writes `Some(code)` and wins from then on; `None` is re-resolved on every launch, so a user who never chose follows OS changes and one who did is never flipped. [H6][Z1]
- Hot switch: `settings::update` (persists via the existing debounce) → `set_locale` → `cx.refresh_windows()` → rebuild native/in-window menus (§9.4).

### 8.2 Why

1. **It is the only i18n crate proven inside GPUI.** gpui-component runs it in a shipping component library, and its documentation already maps the GPUI-specific failure modes — no repaint from `set_locale`, cached menus needing rebuild, CJK glyph/line-height effects, the RTL and data-formatting boundaries (`i18n.md:118-150`) — while its test code shows the one trap the docs omit: the process-global locale leaking between tests (`crates/story/src/app_menus.rs:173-193` resets to `"en"`). Holt copies a working integration instead of rediscovering the traps; Fluent/gettext integrations would be first-of-kind in this framework. [G3][G5]
2. **Minimal dependency delta.** Two small crates — `rust-i18n`, plus `sys-locale` solely for the follow-OS default (Zed's only system-locale dependency, used for exactly this; the vendored snapshot exposes no app-level locale API, only DirectWrite font-locale internals in `gpui_windows`) — no ICU tree, compile-time embedding, no runtime asset IO, nothing new under `~/.holt`, no engine involvement, no change to the vendored snapshot. CODE-STANDARDS requires justification for each new dependency; "the only gpui-proven i18n crate" and "the ecosystem's standard OS-locale probe" are those justifications. [Z1][H3]
3. **Incremental migration with a safe floor.** `fallback = "en"` plus missing-key-returns-key means extraction can proceed surface-by-surface: an unmigrated surface keeps rendering English; a typo'd key is visibly the key string, not a crash or a blank.
4. **zh-CN — the actual requirement — is a flat string-table problem.** Fluent's extra power (CLDR plural categories, selectors, gender) buys nothing at Holt's current message profile (~1,248 unique candidate keys, §7.1) and costs a multi-crate stack plus a translation platform. Note the deliberate divergence from Zed's RFC "embedding is a non-starter": Zed needs a public translation pipeline with external contributors; Holt ships a small fixed language set with the binary, where compile-time embedding is a feature (no runtime file loading, no partial-translation states on disk).

This selection names rejected alternatives, adds a serialized `UiSettings` key, and spans modules — under AGENTS.md's rules it earns a `docs/DECISION.md` entry when implemented.

### 8.3 Rejected alternatives

| Alternative | One-line rejection |
| --- | --- |
| **Status quo** (hardcoded English, Zed's de facto state) | zh-CN is a stated goal, and the retrofit cost grows with every merged literal — Zed's own RFC is stalled on exactly this; at 1,929 sites Holt is at its cheapest migration point, and waiting only re-prices the work upward. [U1][U2] |
| **Fluent stack** (fluent-syntax/bundle/templates + unic-langid, per Zed RFC #43592) | Zero GPUI adopters, unadopted even in Zed; four-plus crates and a Pontoon-class toolchain to solve plural rules Holt's catalog doesn't yet need; Holt would be the first to build the GPUI repaint/menu integration on it. Revisit if CLDR plurals/selectors become a real requirement. [U1] |
| **gettext / .po** | C-centric toolchain, msgid-as-source-string keying, weaker-maintained Rust crates, runtime `.mo` loading or awkward embedding; no GPUI precedent. |
| **Homegrown catalog** (phf/static map + custom `t!` macro) | Reimplements what rust-i18n 4.2 already ships — macro embedding, per-locale lookup, BCP-47 subtag fallback — that gpui-component has already exercised; violates CODE-STANDARDS' "prefer the existing helper" with zero capability gained. |
| **Wait for Zed upstream** | #7409 open since 2024-02 with no plan, timeline, or owner; #43592 is a community proposal. Holt cannot schedule against an unowned backlog. [U1][U2] |
| **Engine-owned locale via new `rpc::methods`** | Language is a device-local UI preference; appearance/font/theme already persist UI-side in `ui-settings.json`; an RPC would add engine surface and serialization for zero capability and contradict the existing precedent (§7.2). |
| **Default `en`, no OS detection** (gpui-component's own default) | Right for a component library that must not impose policy, wrong for an end-user app whose stated driver is zh-CN: the target audience's first launch would be in English with one more setting to find. Avoiding it costs one tiny crate (§8.2). [G2] |
| **Follow the OS only, no persisted choice** (Zed `time_format`'s posture) | Language must be a sticky, user-overridable preference — `appearance` already sets the pattern (OS default, explicit choice persisted). OS-only flips the UI under a user whose system locale differs from their reading preference and leaves no recovery from a wrong detection. [Z3][H6] |
| **Late-binding / locale-aware components** (zbraniecki's RFC position) | An architecture for dynamic per-view languages in a plugin ecosystem; Holt controls every render site and the `refresh_windows` + menu-rebuild path is proven adequate at gpui-component's scale. Not worth the indirection. [U1] |

## 9. Implementation and migration notes

### 9.1 Key organization

Mirror the module layout / surface groups: top-level YAML keys `shell.*`, `settings.*`, `transcript.*`, `composer.*`, `pickers.*`, `files.*`, `changes.*`, `terminal.*`, `notifications.*`, `common.*` (shared verbs like Remove/Retry/Cancel). Dotted nested keys, English values authored inline. Keys are stable identifiers — never rename casually, since zh-CN and en must stay key-aligned; a catalog completeness test (§9.6) enforces it. Keep the theme-name split from §7.1: serialized identity stays `"Orange"` forever; only the label projection goes through `t!`.

### 9.2 Placeholders and plurals

- Keep variable placement inside whole translated messages (`t!("settings.usage.threshold", count = n)`), never concatenate separately translated fragments — gpui-component's docs call this out explicitly, and it is what lets a translator reorder a zh-CN sentence (`i18n.md:150`). [G3]
- rust-i18n has **no CLDR plural rules**. For genuine counts, author two keys (`key.one` / `key.other`) and pick with a small helper at the call site. Do **not** reproduce Zed `time_format`'s hand-composed `"1 year, 10 months ago"` ladder (`time_format.rs:296-322`) — it needed its own regression test (issue #57907) and is the canonical example of plural debt. [Z3]

### 9.3 CJK width, fonts, and fallback

A zh-CN string changes measured widths in both directions: full-width CJK glyphs run wider per character, while translated phrases run shorter or longer than English. Check narrow buttons, menu rows, tooltips, input placeholders, and truncated/ellipsized labels at default and reduced sizes. A font family that *loads* can still lack the glyphs, and the fallback face changes line breaks and line-height (`i18n.md:144`) — verify `ui_font_family`'s CJK coverage in the vendored text system on **all three platforms** (CI runs macOS/Linux/Windows), not just the dev machine. Measure with shaped layout, never character counts. [G3]

### 9.4 Hot switching

`set_locale` repaints nothing (process-global, outside change tracking). The app-level sequence is: `settings::update(SavePolicy::Debounced, ...)` (persist) → `rust_i18n::set_locale(code)` → `cx.refresh_windows()` → rebuild native and in-window menus, because menu labels and checked state are captured at build time, not live lookups (the story `SelectLocale` precedent, `crates/story/src/app_menus.rs:36-49`). Anything else that caches presentation strings at build time (queued notification chips, statically built popovers) needs the same explicit rebuild pass. [G5]

### 9.5 Persistence and serialization

New `UiSettings` field only: `interface_language: Option<String>` (serialized `interfaceLanguage`). The container is already `#[serde(default, rename_all = "camelCase")]`, so old `ui-settings.json` files load unchanged with the field as `None` (= follow OS, §8.1) and no migration code is needed. `Option` rather than a `String` with a default is deliberate: it is the only way to tell "never chose" from "chose English". `None` is resolved at bootstrap and only there — not inside a serde default function, not reactively mid-session — and nothing is written back, so a user who never chose keeps following the OS. Explicit choices ride the existing `SettingsStore` debounce/flush machinery; nothing new hits the disk layer. No RPC, no engine change. [H6]

### 9.6 Testing

- **Catalog completeness is a source scan, not a runtime walk.** `t!` expands at compile time and rust-i18n keeps no registry of referenced keys, so "every key the code uses" can only come from the source text. The check: extract the literal first argument of every `t!("…")` under `crates/ui/src` — anchor on a word boundary (`\bt!\("`), because `format!(` and `assert!(` also end in `t!(` — flatten the YAML to dotted keys, and diff the two sets for each shipped language (`en`, `zh-CN`), as a test or an `xtask`. It is the same text-scan technique as the §7.1 counts, so it is reproducible from a shell. Running exactly this against gpui-component's component crate on 2026-10-24 (`rg -o '\bt!\("[^"]+"' crates/component/src`, YAML flattened with a ten-line script) is both proof of the mechanism and a demonstration of what it catches: **109 literal keys vs 120 catalog keys**, including `combobox.rs:553` asking for `Combobox.placeholder` while the catalog spells the section `ComboBox` — lookup is an exact `HashMap::get` (`rust-i18n-support-4.2.1/src/backend.rs:178-180`; no case folding anywhere in the three rust-i18n crates), so that `t!` renders the key string — and `clipboard.rs:122` calling `t!("Copy")` with no `Copy` key at all, invisible in English because key equals text, silently untranslated in zh-CN. **What the scan cannot see:** non-literal sites — gpui-component has one, `t!($translation)` in `questionnaire/components.rs:1384`, which is why its `Questionnaire.*` keys look unused to the scan — need a hand-maintained allowlist; keep Holt's `t!` calls literal so that list stays empty. The story test `extends_component_translations_with_story_locales` (`crates/story/src/lib.rs:1246-1259`) is a resolved-value **spot check** (fr→Janvier, en→January) — the right pattern for asserting a few sentinel keys per language, not a completeness check. [G4][G5]
- **Process-global pollution**: the locale is a static — tests that switch it must reset to `"en"` on exit (the story app-menu test does exactly this, re-verified at `crates/story/src/app_menus.rs:173-193`), and parallel tests share it. [G5]
- **Real surfaces**: per RETROS ("a re-implementation is not a test"), verify switch behavior on real views with `#[gpui::test]` / `VisualTestContext`, not just key-lookup unit tests — the repaint and menu-rebuild path is where this design can silently fail. [H1]

### 9.7 Migration order and grep discipline

Migrate surface-by-surface, starting with `shell-chrome` (menu bar, app menus, titlebar, notices) and `settings-general-appearance` — highest visibility, bounded scope — then `sidebar-spaces`, `pickers-dialogs`, and inward; `transcript`/`composer` last since they carry conversational-prose rules. Per RETROS' deletion-cleanup lesson, when extracting a surface grep **every** literal form — cross-line strings, `format!` interpolations, tooltip closures, const-spelled sites like `ENGINE_NOT_CONNECTED` (`state.rs:327`, whose offline-styling matcher must keep matching the localized projection) — and per the "every fact needs a landing spot" lesson, move prose into keys without paraphrasing away semantics. [H1][H5]

### 9.8 Compile-time embedding consequences

New languages and new keys require a rebuild — acceptable for Holt's tagged-release cadence, but there is no runtime translation drop, and a partially translated language renders mixed-language UI with English gaps (by design, via fallback). The catalog is code: review it in PRs like code.

### 9.9 Out of scope, explicitly

- **RTL**: neither project mirrors layout for Arabic/Hebrew; the locale API switches strings only (`i18n.md:146,164`). Holt targets zh-CN/en — no mirroring work. [G3]
- **OS-native surfaces**: system notification bodies (`notifications.rs:188-189`) are localized at composition time before posting; anything the OS itself renders (notification titles from the system, file dialogs) is not covered by an in-app catalog.
- **Engine strings**: chat content, markdown rendering, and anything arriving over RPC stays as-is; localization is a `crates/ui` render-time concern.

## 10. Primary-source index

### Holt repository

- [H0] [`ARCHITECTURE.md`](../../ARCHITECTURE.md): the four hard boundaries; `ui-settings.json`'s exceptional status discussed in §7.2.
- [H1] [`docs/RETROS.md`](../RETROS.md): grep-all-forms cleanup, "re-implementation ≠ test", every-fact-has-a-landing-spot lessons.
- [H2] [`third_party/gpui/crates/gpui_shared_string/gpui_shared_string.rs:10-39`](../../third_party/gpui/crates/gpui_shared_string/gpui_shared_string.rs#L10-L39): `SharedString` as a locale-free `SmolStr` wrapper.
- [H3] Repository greps re-run 2026-10-24: `rg -il 'fluent|i18n|localiz|gettext|translation' third_party/gpui --glob '!target'` (all hits are false friends; `FluentBuilder` at `crates/gpui_util/src/lib.rs:1`, used `crates/gpui/src/element.rs:37,159`); `rg -in 'fluent|gettext|locale' third_party/gpui/crates/gpui/Cargo.toml` → exit 1; `rg -l 'gpui-component|gpui_component|gpui-kit|gpui_kit' --glob 'Cargo.toml'` → no match (Holt does not use gpui-component); `rg -n 'sys-locale|sys_locale' --glob 'Cargo.toml'` → no match (new dependency); `rg -n 'fn .*locale|current_locale|get_locale' third_party/gpui/crates --glob '*.rs'` → only `gpui_windows/src/direct_write.rs:1353,1365,1748` font-locale internals, no app-level locale API.
- [H4] String counts re-run 2026-10-24: `find crates/ui -name '*.rs' | wc -l` → 141; `rg -o '"[A-Z][^"]*[a-z][^"]*"' crates/ui/src --glob '*.rs' | wc -l` → 1929; unique → 1248; unique multi-word → 865.
- [H5] [`crates/ui/src/shell/menu_bar.rs:91-92`](../../crates/ui/src/shell/menu_bar.rs#L91-L92), [`crates/ui/src/app_menus.rs:169-177`](../../crates/ui/src/app_menus.rs#L169-L177), [`crates/ui/src/state.rs:327`](../../crates/ui/src/state.rs#L327), [`crates/ui/src/theme.rs:77-83`](../../crates/ui/src/theme.rs#L77-L83): canonical literal shapes and the theme identity/label trap.
- [H6] [`crates/ui/src/settings.rs:55,67-80,252-254,321-327,880,903`](../../crates/ui/src/settings.rs#L67-L80): `SettingsStore` as sole writer, debounce, camelCase serde container, load/save.
- [H7] [`crates/ui/src/lib.rs:100-172`](../../crates/ui/src/lib.rs#L100-L172): `run_app` bootstrap order and the locale mounting point.

### Zed snapshot (clone @ `9f305aaf`, 2026-10-23)

- [Z1] `Cargo.lock:6463` (`fluent-uri`, the only fluent-prefixed package), root `Cargo.toml:920` (`sys-locale`), `git grep -l 'sys-locale' HEAD -- '*/Cargo.toml'` → only `crates/time_format`; `git ls-tree -r --name-only HEAD | grep -c '\.ftl$'` → 0; settings-crate greps for UI-language keys → 0. ([pinned tree](https://github.com/zed-industries/zed/tree/9f305aaf88370e8ca39c702242186e3d8f97ecca))
- [Z2] `crates/ui/src/components/project_empty_state.rs:71`, `crates/zed/src/zed/app_menus.rs:74-75`: hardcoded label/menu literals. ([pinned file](https://github.com/zed-industries/zed/blob/9f305aaf88370e8ca39c702242186e3d8f97ecca/crates/zed/src/zed/app_menus.rs))
- [Z3] `crates/time_format/src/time_format.rs:108-109,263-321,469,482-496,498-573,575-624`: `CURRENT_LOCALE` OnceLock with `"en-US"` fallback (re-read during survey), hand-written plurals, 12/24h whitelist, macOS/Windows OS formatters. ([pinned file](https://github.com/zed-industries/zed/blob/9f305aaf88370e8ca39c702242186e3d8f97ecca/crates/time_format/src/time_format.rs))

### gpui-component snapshot (clone @ `ecd2c8f`, 2026-10-24; repo `longbridge/gpui-component`, docs brand it gpui-kit)

- [G1] `Cargo.toml:102`, `crates/component/Cargo.toml:134`, `crates/story/Cargo.toml:37`: `rust-i18n = "4.2.0"` as the only i18n dependency (re-verified). ([pinned file](https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/Cargo.toml))
- [G2] `crates/component/src/lib.rs:125-160`: `i18n!("locales", fallback = "en")`, locale-free `init()`, `locale()/set_locale()` wrappers (re-verified); `crates/component/locales/ui.yml:1-14` (re-verified).
- [G3] `website/docs/i18n.md` (164 lines; key lines 9, 44, 98, 118-140, 144-150, 158-165 re-read during survey): app-owned locale policy, coverage caveat, change-tracking/repaint rules, CJK/RTL/formatting guidance, common-mistakes table. ([file](https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/website/docs/i18n.md))
- [G4] `crates/component/src/time/calendar.rs:6,15-19`, `pagination.rs:7,119-121`, `combobox.rs:553`, `select.rs:483,523,1061`: `t!` call shapes returning `Cow<str>` → `SharedString`. Key scan re-run 2026-10-24: `rg -o '\bt!\("[^"]+"' crates/component/src | sort -u` → 109 literal keys vs 120 flattened `ui.yml` keys; `clipboard.rs:122` `t!("Copy")` (no catalog key); `questionnaire/components.rs:1384` `t!($translation)` (the only non-literal site); exact-match lookup at `rust-i18n-support-4.2.1/src/backend.rs:178-180`, no case folding in `rust-i18n`/`-support`/`-macro` 4.2.1 (`rg 'to_lowercase|to_ascii_lowercase|eq_ignore_ascii_case|to_uppercase'` → exit 1).
- [G5] `crates/story/src/app_menus.rs:14-21,36-49,173-193` (SelectLocale rebuild; test resetting locale to "en" — re-verified), `crates/story/src/lib.rs:1246-1259` (override-precedence test), `~/.cargo/registry/.../rust-i18n-macro-4.2.1/src/lib.rs:360-395` (`extend!`/NamespacedBackend, single-extend assert, `_rust_i18n_lookup_fallback`; presence re-checked).

### External

- [U1] [Zed discussion #43592 — Localization/i18n RFC](https://github.com/zed-industries/zed/discussions/43592) (2025-11-26, unadopted community proposal; embedding-is-a-non-starter, key strategies, late binding, fluent-langneg/Pontoon).
- [U2] [Zed issue #7409 — Localization tracking](https://github.com/zed-industries/zed/issues/7409) (open since 2024-02, no plan or owner).
- [U3] [Zed discussion #56838](https://github.com/zed-industries/zed/discussions/56838), [discussion #64167](https://github.com/zed-industries/zed/discussions/64167) (unanswered zh-CN requests); community forks [x6nux/zed-globalization](https://github.com/x6nux/zed-globalization) and [axxion/Zed-L10n](https://github.com/axxion/Zed-L10n) (build-time patched binaries).

[H0]: ../../ARCHITECTURE.md
[H1]: ../RETROS.md
[H2]: ../../third_party/gpui/crates/gpui_shared_string/gpui_shared_string.rs#L10-L39
[H3]: ../../third_party/gpui/crates/gpui_util/src/lib.rs#L1
[H4]: ../../crates/ui/src
[H5]: ../../crates/ui/src/app_menus.rs#L169-L177
[H6]: ../../crates/ui/src/settings.rs#L67-L80
[H7]: ../../crates/ui/src/lib.rs#L100-L172
[Z1]: https://github.com/zed-industries/zed/tree/9f305aaf88370e8ca39c702242186e3d8f97ecca
[Z2]: https://github.com/zed-industries/zed/blob/9f305aaf88370e8ca39c702242186e3d8f97ecca/crates/ui/src/components/project_empty_state.rs#L71
[Z3]: https://github.com/zed-industries/zed/blob/9f305aaf88370e8ca39c702242186e3d8f97ecca/crates/time_format/src/time_format.rs#L108-L109
[G1]: https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/Cargo.toml#L102
[G2]: https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/crates/component/src/lib.rs#L125-L160
[G3]: https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/website/docs/i18n.md
[G4]: https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/crates/component/src/time/calendar.rs#L6-L19
[G5]: https://github.com/longbridge/gpui-component/blob/ecd2c8ff42439b73b87a238d02db10fe20376740/crates/story/src/app_menus.rs#L14-L49
[U1]: https://github.com/zed-industries/zed/discussions/43592
[U2]: https://github.com/zed-industries/zed/issues/7409
[U3]: https://github.com/zed-industries/zed/discussions/56838
