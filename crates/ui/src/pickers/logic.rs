//! Pure picker logic and domain types (feature-inventory §1.7): the draft
//! config the pickers accumulate, checkout-plan types, default/traits
//! resolution, folder-browser navigation, model-list normalization, provider
//! flattening, and provider brand icons. No GPUI types live here so catalog
//! and checkout behavior stays cheap to test; the [`super`] facade re-exports
//! every public name at `crate::pickers`.

use holt_proto::{
    ChatConfig, FolderListing, Model, PermissionMode, Provider, ProviderId, ReasoningLevel,
};

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// comes from the selected SPACE — the draft only carries the git extras (ref
/// + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The picked ref (base branch in NewWorktree mode). `None` = the
    /// repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs.
    pub checkout: CheckoutKind,
}

/// Where a new session runs: the space's own folder, or a fresh worktree
/// minted on send. A ref already materialized as a worktree is never a
/// target — that worktree joins holt as its own Space (ADR-0007).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — always the new chat's working directory.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`super::Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// `CreateWorktree` off `base` on send (holt mints a `holt/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete provider,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known provider).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            provider: self.provider.clone()?,
            model: self.model.clone()?,
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            permission_mode: PermissionMode::default(),
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The provider's default model: the first catalog row (both curated catalogs
/// lead with the flagship — holt's `pickDefaultModel` Opus preference maps to
/// the same row here).
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// A model's default reasoning: X-High when the ladder offers it (holt
/// `DEFAULT_REASONING = "xhigh"`), else High, else the ladder's first entry.
/// `None` only for ladder-less models (e.g. Haiku's thinking toggle instead).
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it, else fall to the model's default (never a stale
/// or foreign level — holt use-run-config.ts's derived-model discipline).
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// The TraitsPicker trigger summary: the effective reasoning level plus every
/// model option's effective choice — the explicit pick when one is saved and
/// still offered, else the option's default — joined with " · " ("High · 1M ·
/// Fast", Cursor's "Agent · Balance"). Defaults are spelled out rather than
/// hidden so the run's configuration reads without opening the popover; `None`
/// only when the model has nothing to describe (no ladder, no options).
pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let choice_id = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .filter(|id| option.choices.iter().any(|c| c.id == *id))
                .unwrap_or(&option.default_choice);
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Whether any trait departs from its default — the trigger brightens only
/// then, so a customized run still stands out now that the summary always
/// names the effective choices.
pub fn traits_customized(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
    selections: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    if reasoning != default_reasoning(ladder) {
        return true;
    }
    model.is_some_and(|model| {
        model.options.iter().any(|option| {
            selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .is_some_and(|id| {
                    id != option.default_choice && option.choices.iter().any(|c| c.id == id)
                })
        })
    })
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The create row's input validation, as a pure function: the submit
/// affordance is enabled only for a name that is non-empty after trimming,
/// and the submitted name is that trimmed value. Everything past this
/// (ref-format legality, duplicates) is git's to judge.
pub(crate) fn branch_create_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Pure: the switch-failure dialog (ADR-0007)
// ---------------------------------------------------------------------------

/// Content of the switch-failure dialog: inform-only — a short explanation
/// plus the blocking file list when the refusal carried one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchDialogContent {
    pub title: String,
    pub message: String,
    /// Blocking file paths, engine-sorted, one per line after the marker.
    pub files: Vec<String>,
}

/// The engine's dirty-tree refusal marker: the error's first line, with the
/// blocking paths on the lines after it. Shared with the engine through
/// [`holt_proto::SWITCH_REFUSAL_MARKER`] — one definition, tests pinning the
/// shape on both sides.
pub(crate) use holt_proto::SWITCH_REFUSAL_MARKER;

/// Assemble the dialog content from a `SwitchRef`/`CreateBranch` failure.
/// A dirty-tree refusal explains the blocked files and what to do about
/// them; any other failure — git's own worktree refusal, a transport error
/// — surfaces verbatim with no file list. The dialog only informs: there is
/// no force/stash/discard arm anywhere.
pub(crate) fn switch_dialog_content(error_message: &str) -> SwitchDialogContent {
    if let Some(rest) = error_message.strip_prefix(SWITCH_REFUSAL_MARKER) {
        let files = rest
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        return SwitchDialogContent {
            title: "Couldn't switch branches".into(),
            message: "The switch was refused to protect uncommitted changes. \
Commit or stash these files, then switch again:"
                .into(),
            files,
        };
    }
    SwitchDialogContent {
        title: "Couldn't switch branches".into(),
        message: error_message.to_string(),
        files: Vec::new(),
    }
}

/// The worktree-hosted pick dialog: the ref is checked out in another
/// worktree, so it can never be switched to — that worktree joins holt as
/// its own Space (ADR-0007). Guidance, not a dead end.
pub(crate) fn worktree_hosted_dialog(branch: &str, path: &str) -> SwitchDialogContent {
    SwitchDialogContent {
        title: "Can't switch to that branch".into(),
        message: format!(
            "{branch} is checked out in another worktree ({path}). To work on it there, \
add that folder as its own project.",
        ),
        files: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Pure: pick routing + session footer labels (ADR-0007)
// ---------------------------------------------------------------------------

/// Which surface a ref-pick happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickSurface {
    /// An existing chat: picks safe-switch the chat's working directory.
    Session,
    /// The new-chat draft: picks configure the chat-to-be.
    Draft,
}

/// What picking a ref row does (ADR-0007): the branch is live
/// working-directory state, so a session pick switches immediately — a
/// Turn is never interrupted, the next Turn runs on whatever the folder
/// holds when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickRouting {
    /// The ref is already the working directory's branch: close, no git.
    AlreadyCurrent,
    /// Record the pick as draft state (a new-worktree base, or naming the
    /// current ref on the draft chip). No git.
    RecordPick,
    /// Safe-switch the target working directory right away.
    Switch,
    /// The ref is checked out in another worktree: raise the import-as-a-
    /// Space dialog instead.
    WorktreeHosted,
}

/// Route one ref-pick. `new_worktree_mode` is the draft's checkout kind
/// (`NewWorktree` makes every pick a base pick); sessions ignore it —
/// their checkout kind is fixed and display-only. One rule everywhere: a
/// ref hosted in another worktree is never a target, draft or session —
/// except a working directory's OWN branch, which carries both the
/// `current` and the worktree flag and must read as current.
pub(crate) fn pick_routing(
    surface: PickSurface,
    new_worktree_mode: bool,
    row: &holt_proto::RepoRef,
) -> PickRouting {
    if row.current {
        return PickRouting::AlreadyCurrent;
    }
    if row.worktree_path.is_some() {
        return PickRouting::WorktreeHosted;
    }
    match surface {
        PickSurface::Session => PickRouting::Switch,
        PickSurface::Draft => {
            if new_worktree_mode {
                PickRouting::RecordPick
            } else {
                PickRouting::Switch
            }
        }
    }
}

/// Whether a session's working directory is a legacy worktree (cwd away
/// from the space folder) — the one rule behind both the footer kind icon
/// and its label.
pub(crate) fn session_runs_in_worktree(space_path: &str, chat_cwd: Option<&str>) -> bool {
    chat_cwd.is_some_and(|cwd| cwd != space_path)
}

/// The session footer's checkout-kind label (display-only in sessions —
/// the "New worktree" option is a draft-only affordance): a legacy
/// worktree chat (cwd away from the space folder) reads "Worktree"; every
/// other chat runs in its space's folder.
pub(crate) fn session_checkout_label(space_path: &str, chat_cwd: Option<&str>) -> &'static str {
    if session_runs_in_worktree(space_path, chat_cwd) {
        "Worktree"
    } else {
        "Local checkout"
    }
}

/// The session branch chip's label: the working directory's live current
/// branch when the refs cache knows it, else the chat's stamped branch
/// (its latest Turn's, per ADR-0007), else the placeholder.
pub(crate) fn session_branch_label(live: Option<&str>, stamped: Option<&str>) -> String {
    live.or(stamped).unwrap_or("No ref").to_string()
}

/// Byte length of `name`'s prefix matching `query`, compared char-for-char
/// case-insensitively; `None` when `query` isn't a prefix of `name`. The
/// length indexes into `name` (not `query`) so the completion suffix keeps
/// the folder's real casing: `("Documents", "doc") → Some(3)` → `"uments"`.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder `names` (slash-descend):
/// exact match first — case-SENSITIVE before case-insensitive, so `GitHub/`
/// picks a `GitHub` sibling over `github` — then a unique case-insensitive
/// prefix. Ambiguity resolves to `None`: the slash stays in the query.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

/// Interpret a palette query as a typed path jump: absolute (`/disk2/projects`)
/// or home-relative (`~`, `~/github`). Returns the absolute path to browse,
/// trailing slash trimmed. `home` is the local machine's resolved home —
/// `None` until the first listing lands, when `~` can't expand yet. A query
/// like `~foo` is a folder name, not a path.
pub fn typed_path_target(query: &str, home: Option<&str>) -> Option<String> {
    let query = query.trim();
    if let Some(rest) = query.strip_prefix('~') {
        let home = home?.trim_end_matches('/');
        if rest.is_empty() {
            return Some(home.to_string());
        }
        let rest = rest.strip_prefix('/')?.trim_end_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if query.starts_with('/') {
        let trimmed = query.trim_end_matches('/');
        return Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
    }
    None
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&holt_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Pure: catalog hygiene + flattening
// ---------------------------------------------------------------------------

/// Display-side model-list hygiene for backend-served catalogs: the
/// `default` alias row drops when a real row exists, and an orphan
/// `<model>[1m]` variant presents as its base id with the Context Window
/// trait pinned to 1M. Idempotent over already-clean lists. The send path
/// recomposes the advertised id from the base + trait (`pick_model_value`),
/// so a folded pick still runs.
pub(crate) fn normalize_model_rows(models: Vec<Model>) -> Vec<Model> {
    fn strip_1m(id: &str) -> Option<&str> {
        id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
    }
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let has_real = ids.iter().any(|id| !id.eq_ignore_ascii_case("default"));
    models
        .into_iter()
        .filter_map(|mut model| {
            if has_real && model.id.eq_ignore_ascii_case("default") {
                return None;
            }
            if let Some(base) = strip_1m(&model.id.clone()) {
                if ids.iter().any(|other| other == base) {
                    // The bare base is listed too — the engine already gave
                    // it the Context Window trait; the variant row is noise.
                    return None;
                }
                model.id = base.to_string();
                // "Opus (1M context)" → "Opus".
                if let Some(at) = model.label.rfind(" (")
                    && model.label.ends_with(')')
                {
                    model.label.truncate(at);
                    while model.label.ends_with(' ') {
                        model.label.pop();
                    }
                }
                if !model.options.iter().any(|o| o.id == "contextWindow") {
                    model.options.push(holt_proto::ModelOption {
                        id: "contextWindow".into(),
                        label: "Context Window".into(),
                        choices: vec![
                            holt_proto::ModelOptionChoice {
                                id: "200k".into(),
                                label: "200K".into(),
                            },
                            holt_proto::ModelOptionChoice {
                                id: "1m".into(),
                                label: "1M".into(),
                            },
                        ],
                        default_choice: "1m".into(),
                    });
                }
            }
            Some(model)
        })
        .collect()
}

/// Providers available to the composer: every configured variant, flattened
/// from organization rows back into concrete providers. The picker keeps
/// `provider/model` addressing — organization grouping lives on the settings
/// page, so a configured `minimax-cn` is offered even when its `minimax`
/// sibling has no key.
pub fn offered_providers(list: &[Provider]) -> Vec<Provider> {
    list.iter()
        .flat_map(|row| row.concrete_providers())
        .filter(|provider| provider.configured)
        .collect()
}

// ---------------------------------------------------------------------------
// Pure: provider brand icons
// ---------------------------------------------------------------------------

/// The appearance-aware icon path for a provider (the GPUI-tinted wrapper
/// lives in `provider_model`; this stays free of GPUI types).
pub(super) fn provider_brand_icon_for(
    provider: &ProviderId,
    appearance: crate::theme::Appearance,
) -> Option<&'static str> {
    use crate::icons;

    let dark = appearance.is_dark();
    Some(match provider.as_str() {
        "ant-ling" => icons::PROVIDER_ANT_LING,
        "anthropic" => {
            if dark {
                icons::PROVIDER_ANTHROPIC_DARK
            } else {
                icons::PROVIDER_ANTHROPIC_LIGHT
            }
        }
        "baseten" => icons::PROVIDER_BASETEN,
        "cerebras" => icons::PROVIDER_CEREBRAS,
        "deepseek" => icons::PROVIDER_DEEPSEEK,
        "fireworks" => icons::PROVIDER_FIREWORKS,
        "github-copilot" => icons::PROVIDER_GITHUB_COPILOT,
        "google" => icons::PROVIDER_GOOGLE,
        "groq" => icons::PROVIDER_GROQ,
        "huggingface" => icons::PROVIDER_HUGGINGFACE,
        "kimi-coding" | "moonshot-kimi" => icons::PROVIDER_KIMI_CODING,
        "minimax" | "minimax-cn" => {
            if dark {
                icons::PROVIDER_MINIMAX_DARK
            } else {
                icons::PROVIDER_MINIMAX_LIGHT
            }
        }
        "mistral" => icons::PROVIDER_MISTRAL,
        "moonshot" | "moonshotai" | "moonshotai-cn" => icons::PROVIDER_MOONSHOTAI,
        "nvidia" => icons::PROVIDER_NVIDIA,
        "openai" => {
            if dark {
                // This supplied variant is the white mark for dark surfaces.
                icons::PROVIDER_OPENAI_LIGHT
            } else {
                icons::PROVIDER_OPENAI
            }
        }
        "opencode" | "opencode-go" => {
            if dark {
                icons::PROVIDER_OPENCODE_DARK
            } else {
                icons::PROVIDER_OPENCODE_LIGHT
            }
        }
        "openrouter" => icons::PROVIDER_OPENROUTER,
        "qwen" | "qwen-token-plan" | "qwen-token-plan-cn" | "qwen-token-plan-individual" => {
            icons::PROVIDER_QWEN
        }
        "together" => icons::PROVIDER_TOGETHER,
        "vercel-ai-gateway" => icons::PROVIDER_VERCEL_AI_GATEWAY,
        "xai" => icons::PROVIDER_XAI,
        "xiaomi" | "xiaomi-token-plan-ams" | "xiaomi-token-plan-cn" | "xiaomi-token-plan-sgp" => {
            icons::PROVIDER_XIAOMI
        }
        "zai" | "zai-coding-cn" => icons::PROVIDER_ZAI,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::ProviderVariant;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.to_string(),
            provider: ProviderId("test".into()),
            label: label.to_string(),
            description: None,
            reasoning_levels: Vec::new(),
            default_reasoning: None,
            options: Vec::new(),
            custom: false,
        }
    }

    #[test]
    fn every_eligible_provider_has_a_brand_icon() {
        let ids = [
            "ant-ling",
            "anthropic",
            "baseten",
            "cerebras",
            "deepseek",
            "fireworks",
            "github-copilot",
            "google",
            "groq",
            "huggingface",
            "kimi-coding",
            "minimax",
            "minimax-cn",
            "mistral",
            "moonshotai",
            "moonshotai-cn",
            "nvidia",
            "opencode",
            "opencode-go",
            "openai",
            "openrouter",
            "qwen-token-plan",
            "qwen-token-plan-cn",
            "qwen-token-plan-individual",
            "together",
            "vercel-ai-gateway",
            "xai",
            "xiaomi",
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-sgp",
            "zai",
            "zai-coding-cn",
        ];
        for id in ids {
            let provider = ProviderId(id.into());
            assert!(
                provider_brand_icon_for(&provider, crate::theme::Appearance::Light).is_some(),
                "missing light icon for {id}"
            );
            assert!(
                provider_brand_icon_for(&provider, crate::theme::Appearance::Dark).is_some(),
                "missing dark icon for {id}"
            );
        }
    }

    #[test]
    fn themed_provider_icons_select_the_matching_variant() {
        let anthropic = ProviderId("anthropic".into());
        assert_eq!(
            provider_brand_icon_for(&anthropic, crate::theme::Appearance::Light),
            Some(crate::icons::PROVIDER_ANTHROPIC_LIGHT)
        );
        assert_eq!(
            provider_brand_icon_for(&anthropic, crate::theme::Appearance::Dark),
            Some(crate::icons::PROVIDER_ANTHROPIC_DARK)
        );
        assert!(
            provider_brand_icon_for(
                &ProviderId("future-provider".into()),
                crate::theme::Appearance::Dark
            )
            .is_none()
        );
    }

    #[test]
    fn reasoning_defaults_to_high_when_supported() {
        assert_eq!(
            default_reasoning(&[ReasoningLevel::Low, ReasoningLevel::High]),
            Some(ReasoningLevel::High)
        );
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn only_configured_variants_are_offered() {
        let variant = |id: &str, configured: bool| ProviderVariant {
            id: ProviderId(id.into()),
            name: id.into(),
            configured,
        };
        let row = |id: &str, variants: Vec<ProviderVariant>| Provider {
            id: ProviderId(id.into()),
            name: id.into(),
            abbreviation: id[..2].to_ascii_uppercase(),
            configured: variants.iter().any(|v| v.configured),
            variants,
        };
        let providers = vec![
            row("openai", vec![variant("openai", true)]),
            row("anthropic", vec![variant("anthropic", false)]),
            row(
                "minimax",
                vec![variant("minimax", false), variant("minimax-cn", true)],
            ),
        ];
        let offered = offered_providers(&providers);
        let ids: Vec<&str> = offered.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["openai", "minimax-cn"]);
        // The flattened row keeps the variant's own display name.
        assert_eq!(offered[1].name, "minimax-cn");
    }

    #[test]
    fn unconfigured_organizations_offer_nothing() {
        let variant = |id: &str, configured: bool| ProviderVariant {
            id: ProviderId(id.into()),
            name: id.into(),
            configured,
        };
        let org = Provider {
            id: ProviderId("minimax".into()),
            name: "MiniMax".into(),
            abbreviation: "MM".into(),
            configured: false,
            variants: vec![variant("minimax", false), variant("minimax-cn", false)],
        };
        assert!(offered_providers(&[org]).is_empty());
        // A standalone configured row flattens to itself.
        let standalone = Provider {
            id: ProviderId("openai".into()),
            name: "OpenAI".into(),
            abbreviation: "OA".into(),
            configured: true,
            variants: vec![variant("openai", true)],
        };
        let offered = offered_providers(&[standalone]);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].id.as_str(), "openai");
        assert!(offered[0].variants.is_empty());
    }

    #[test]
    fn resolved_config_requires_provider_and_model() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.provider = Some("openai".into());
        resolved.model = Some("openai/gpt-5.4".into());
        assert_eq!(resolved.chat_config().unwrap().model, "openai/gpt-5.4");
    }

    #[test]
    fn chat_config_carries_reasoning_options_and_default_permission_mode() {
        let mut resolved = ResolvedRunConfig {
            provider: Some(ProviderId("openai".into())),
            model: Some("openai/gpt-5.4".into()),
            reasoning: Some(ReasoningLevel::High),
            model_options: {
                let mut map = serde_json::Map::new();
                map.insert(
                    "speed".to_string(),
                    serde_json::Value::String("fast".into()),
                );
                map
            },
        };
        let config = resolved.chat_config().expect("provider + model known");
        assert_eq!(config.model, "openai/gpt-5.4");
        assert_eq!(config.reasoning, Some(ReasoningLevel::High));
        assert_eq!(
            config.model_options.get("speed"),
            Some(&serde_json::Value::String("fast".into()))
        );
        assert_eq!(config.permission_mode, PermissionMode::ConfirmChanges);
        // Model missing: nothing safe to record.
        resolved.model = None;
        assert!(resolved.chat_config().is_none());
    }

    #[test]
    fn branch_create_name_trims_and_rejects_empty() {
        // Empty and whitespace-only inputs never enable submit.
        assert_eq!(branch_create_name(""), None);
        assert_eq!(branch_create_name("   "), None);
        assert_eq!(branch_create_name("\t\n"), None);
        // Real names submit as their trimmed value.
        assert_eq!(branch_create_name("feat/x").as_deref(), Some("feat/x"));
        assert_eq!(
            branch_create_name("  fix-engine  ").as_deref(),
            Some("fix-engine")
        );
    }

    // ---- switch-failure dialog assembly ----

    #[test]
    fn a_refusal_parses_into_message_and_sorted_file_list() {
        let content = switch_dialog_content(
            "switch refused: uncommitted changes would be overwritten by checkout:\nREADME.md\ndelta.txt",
        );
        assert_eq!(content.title, "Couldn't switch branches");
        assert_eq!(
            content.files,
            vec!["README.md".to_string(), "delta.txt".to_string()]
        );
        assert!(
            content.message.contains("uncommitted changes"),
            "the copy explains the refusal: {}",
            content.message
        );
        assert!(
            content.message.contains("Commit or stash"),
            "the copy says what to do next: {}",
            content.message
        );
    }

    #[test]
    fn any_other_failure_surfaces_verbatim_with_no_file_list() {
        // git's own worktree refusal — the other refusal family.
        let content = switch_dialog_content(
            "cannot set HEAD to reference 'refs/heads/feature' as it is the current HEAD of a linked repository.",
        );
        assert_eq!(content.files, Vec::<String>::new());
        assert!(content.message.contains("linked repository"));
        // A transport/engine failure.
        let content = switch_dialog_content("the engine is unreachable");
        assert_eq!(content.files, Vec::<String>::new());
        assert_eq!(content.message, "the engine is unreachable");
        // Both keep the shared title so the surface reads as one dialog.
        assert_eq!(content.title, "Couldn't switch branches");
    }

    #[test]
    fn a_marker_without_files_still_dialogs_without_a_list() {
        // The engine never emits this, but a mid-format change must not
        // render an empty list frame.
        let content = switch_dialog_content(
            "switch refused: uncommitted changes would be overwritten by checkout:",
        );
        assert_eq!(content.files, Vec::<String>::new());
        assert!(content.message.contains("uncommitted changes"));
    }

    // ---- pick routing + session footer labels ----

    fn ref_row(name: &str, current: bool, worktree_path: Option<&str>) -> holt_proto::RepoRef {
        holt_proto::RepoRef {
            name: name.into(),
            current,
            worktree_path: worktree_path.map(str::to_string),
        }
    }

    #[test]
    fn session_picks_switch_except_current_and_worktree_hosted() {
        // A plain, non-current ref: immediate safe-switch of the chat's
        // working directory.
        assert_eq!(
            pick_routing(
                PickSurface::Session,
                false,
                &ref_row("feature", false, None)
            ),
            PickRouting::Switch
        );
        // The already-current ref just closes — no git at all.
        assert_eq!(
            pick_routing(PickSurface::Session, false, &ref_row("main", true, None)),
            PickRouting::AlreadyCurrent
        );
        // A ref hosted in another worktree: the import-as-a-Space dialog.
        assert_eq!(
            pick_routing(
                PickSurface::Session,
                false,
                &ref_row("feature", false, Some("/wt/feature"))
            ),
            PickRouting::WorktreeHosted
        );
        // A legacy worktree chat's OWN branch carries both current and a
        // worktree path — current wins (switching to it is a no-op close,
        // never the dialog).
        assert_eq!(
            pick_routing(
                PickSurface::Session,
                false,
                &ref_row("feature", true, Some("/wt/self"))
            ),
            PickRouting::AlreadyCurrent
        );
        // The draft-only new-worktree mode does not leak into sessions.
        assert_eq!(
            pick_routing(PickSurface::Session, true, &ref_row("feature", false, None)),
            PickRouting::Switch
        );
    }

    #[test]
    fn draft_picks_switch_or_record_but_never_target_a_worktree() {
        // Local mode + a plain non-current ref: the draft switches the
        // space's folder, exactly like a session.
        assert_eq!(
            pick_routing(PickSurface::Draft, false, &ref_row("feature", false, None)),
            PickRouting::Switch
        );
        // New-worktree mode: every pick is a base pick.
        assert_eq!(
            pick_routing(PickSurface::Draft, true, &ref_row("feature", false, None)),
            PickRouting::RecordPick
        );
        // The current ref is a no-op close on every surface.
        assert_eq!(
            pick_routing(PickSurface::Draft, false, &ref_row("main", true, None)),
            PickRouting::AlreadyCurrent
        );
        // A worktree-hosted ref raises the import-as-a-Space dialog in the
        // draft too — one rule everywhere (ADR-0007; the reuse arm is gone).
        assert_eq!(
            pick_routing(
                PickSurface::Draft,
                false,
                &ref_row("feature", false, Some("/wt/feature"))
            ),
            PickRouting::WorktreeHosted
        );
        // Even in new-worktree mode: the dialog, never a target.
        assert_eq!(
            pick_routing(
                PickSurface::Draft,
                true,
                &ref_row("feature", false, Some("/wt/feature"))
            ),
            PickRouting::WorktreeHosted
        );
    }

    #[test]
    fn session_footer_labels_split_local_worktree_and_live_branch() {
        // A chat in its space's folder.
        assert_eq!(
            session_checkout_label("/space/repo", Some("/space/repo")),
            "Local checkout"
        );
        // A chat whose cwd never arrived yet: the space folder is the
        // default the engine stamps.
        assert_eq!(
            session_checkout_label("/space/repo", None),
            "Local checkout"
        );
        // A legacy worktree chat (cwd ≠ space folder).
        assert_eq!(
            session_checkout_label("/space/repo", Some("/wt/feature")),
            "Worktree"
        );
        // The branch chip prefers the live current branch, falls to the
        // chat's stamped branch, then the placeholder.
        assert_eq!(
            session_branch_label(Some("feature"), Some("main")),
            "feature"
        );
        assert_eq!(session_branch_label(None, Some("main")), "main");
        assert_eq!(session_branch_label(None, None), "No ref");
    }

    #[test]
    fn worktree_hosted_dialog_points_at_importing_the_space() {
        let content = worktree_hosted_dialog("feature", "/wt/feature");
        assert_eq!(content.title, "Can't switch to that branch");
        assert!(content.message.contains("feature"), "{}", content.message);
        assert!(content.message.contains("/wt/feature"));
        assert!(
            content.message.to_lowercase().contains("project"),
            "the guidance names the import path: {}",
            content.message
        );
        assert!(content.files.is_empty());
    }

    // ---- checkout draft semantics ----

    // `Pickers::checkout_plan` itself needs a GPUI entity (Entity/Subscription
    // fields), which plain unit tests can't construct; its decision inputs —
    // `CheckoutKind::default`, `DraftConfig::default`, and the `CheckoutPlan`
    // variant shapes the composer matches on — are pure and covered here.
    #[test]
    fn draft_defaults_start_local_with_no_ref_pick() {
        let draft = DraftConfig::default();
        assert_eq!(draft.provider, None);
        assert_eq!(draft.model, None);
        assert_eq!(draft.reasoning, None);
        assert!(draft.model_options.is_empty());
        assert_eq!(draft.branch, None);
        assert_eq!(draft.checkout, CheckoutKind::default());
        assert_eq!(draft.checkout, CheckoutKind::Local);
    }

    #[test]
    fn checkout_plan_variants_carry_their_payloads() {
        // NewWorktree with refs never loaded: `base: None` — send falls back
        // to the space folder rather than failing.
        assert_eq!(
            CheckoutPlan::NewWorktree { base: None },
            CheckoutPlan::NewWorktree { base: None }
        );
        assert_eq!(
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            },
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            }
        );
        assert_eq!(
            CheckoutPlan::CurrentCheckout { branch: None },
            CheckoutPlan::CurrentCheckout { branch: None }
        );
        assert_eq!(
            CheckoutPlan::CurrentCheckout {
                branch: Some("feat".into())
            },
            CheckoutPlan::CurrentCheckout {
                branch: Some("feat".into())
            }
        );
        // Two arms only — the reuse-worktree arm is deleted (ADR-0007): a
        // new chat's working directory is always its space's folder, and
        // composer/send.rs matches on exactly these discriminants.
        assert_ne!(
            CheckoutPlan::CurrentCheckout { branch: None },
            CheckoutPlan::NewWorktree { base: None }
        );
    }

    // ---- model normalization ----

    #[test]
    fn default_alias_row_drops_only_when_a_real_row_exists() {
        let models = vec![model("default", "Default"), model("gpt-5", "GPT 5")];
        let ids: Vec<String> = normalize_model_rows(models)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["gpt-5"]);
        // A catalog that only offers the alias keeps it.
        let only_alias = vec![model("DEFAULT", "Default")];
        let ids: Vec<String> = normalize_model_rows(only_alias)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["DEFAULT"]);
    }

    #[test]
    fn orphan_1m_variant_folds_onto_its_base_with_a_pinned_trait() {
        let mut variant = model("glm-5[1m]", "GLM 5 (1M context)");
        variant.description = None;
        let folded = normalize_model_rows(vec![variant]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].id, "glm-5");
        assert_eq!(folded[0].label, "GLM 5");
        let option = folded[0]
            .options
            .iter()
            .find(|o| o.id == "contextWindow")
            .expect("contextWindow trait pinned");
        assert_eq!(option.default_choice, "1m");
        let choice_ids: Vec<&str> = option.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(choice_ids, ["200k", "1m"]);
        // The `-1m` suffix folds the same way, and an existing trait is not
        // duplicated.
        let mut dashed = model("kimi-k2-1m", "Kimi K2 (1M context)");
        dashed.options = vec![holt_proto::ModelOption {
            id: "contextWindow".into(),
            label: "Existing".into(),
            choices: Vec::new(),
            default_choice: "1m".into(),
        }];
        let folded = normalize_model_rows(vec![dashed]);
        assert_eq!(folded[0].id, "kimi-k2");
        assert_eq!(
            folded[0]
                .options
                .iter()
                .filter(|o| o.id == "contextWindow")
                .count(),
            1
        );
        assert_eq!(folded[0].options[0].label, "Existing");
    }

    #[test]
    fn variant_row_is_noise_when_the_bare_base_is_listed() {
        let models = vec![
            model("glm-5", "GLM 5"),
            model("glm-5[1m]", "GLM 5 (1M context)"),
        ];
        let ids: Vec<String> = normalize_model_rows(models)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["glm-5"]);
    }

    #[test]
    fn normalization_is_idempotent() {
        let models = vec![
            model("default", "Default"),
            model("glm-5[1m]", "GLM 5 (1M context)"),
            model("kimi-k2-1m", "Kimi K2 (1M context)"),
        ];
        let once = normalize_model_rows(models);
        let twice = normalize_model_rows(once.clone());
        assert_eq!(once, twice);
    }

    // ---- path helpers ----

    #[test]
    fn parent_path_stops_at_the_filesystem_root() {
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(parent_path("/a"), Some("/".to_string()));
        assert_eq!(parent_path("/a/b"), Some("/a".to_string()));
        assert_eq!(parent_path("/a/b/"), Some("/a".to_string()));
        assert_eq!(parent_path("a/b"), Some("a".to_string()));
        assert_eq!(parent_path("a"), None);
    }

    #[test]
    fn child_path_joins_without_doubling_the_separator() {
        assert_eq!(child_path("/Users/x", "src"), "/Users/x/src");
        assert_eq!(child_path("/", "Users"), "/Users");
        assert_eq!(child_path("/Users/x/", "src"), "/Users/x/src");
    }

    #[test]
    fn completion_prefix_len_case_folds_and_indexes_the_name() {
        // The length indexes `name`, keeping the folder's real casing.
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        // The length counts NAME bytes matched — here "doc" inside
        // "documents" — not the whole name.
        assert_eq!(completion_prefix_len("documents", "DOC"), Some(3));
        assert_eq!(completion_prefix_len("abc", ""), Some(0));
        assert_eq!(completion_prefix_len("abc", "abd"), None);
        assert_eq!(completion_prefix_len("ab", "abc"), None);
    }

    #[test]
    fn segment_target_prefers_exact_then_unique_prefix() {
        let names = ["GitHub", "github", "GitLab"];
        // Case-SENSITIVE exact match wins over the ci sibling.
        assert_eq!(segment_target(&names, "GitHub"), Some(0));
        assert_eq!(segment_target(&names, "github"), Some(1));
        // Full-length case-insensitive match resolves.
        assert_eq!(segment_target(&names, "gitlab"), Some(2));
        // Unique prefix resolves.
        assert_eq!(segment_target(&names, "gitl"), Some(2));
        // Ambiguous prefix keeps the slash in the query.
        assert_eq!(segment_target(&names, "git"), None);
    }

    #[test]
    fn typed_path_target_expands_home_and_absolute_paths() {
        let home = "/Users/x";
        assert_eq!(typed_path_target("~", Some(home)), Some("/Users/x".into()));
        assert_eq!(typed_path_target("~/", Some(home)), Some("/Users/x".into()));
        assert_eq!(
            typed_path_target("~/github/", Some(home)),
            Some("/Users/x/github".into())
        );
        // `~foo` is a folder name, not a path; `~` needs a resolved home.
        assert_eq!(typed_path_target("~foo", Some(home)), None);
        assert_eq!(typed_path_target("~", None), None);
        assert_eq!(
            typed_path_target("/disk2/projects/", None),
            Some("/disk2/projects".into())
        );
        assert_eq!(typed_path_target("/", None), Some("/".into()));
        assert_eq!(typed_path_target("relative/path", Some(home)), None);
    }

    #[test]
    fn breadcrumbs_accumulate_root_first() {
        let crumbs = breadcrumbs("/a/b/c");
        let pairs: Vec<(&str, &str)> = crumbs
            .iter()
            .map(|(label, path)| (label.as_str(), path.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("/", "/"), ("a", "/a"), ("b", "/a/b"), ("c", "/a/b/c")]
        );
    }

    #[test]
    fn browser_rows_show_directories_only() {
        let listing = holt_proto::FolderListing {
            path: "/".into(),
            entries: vec![
                holt_proto::FolderEntry {
                    name: "src".into(),
                    is_dir: true,
                    is_repo: false,
                },
                holt_proto::FolderEntry {
                    name: "README.md".into(),
                    is_dir: false,
                    is_repo: false,
                },
                holt_proto::FolderEntry {
                    name: ".git".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        let names: Vec<&str> = browser_rows(&listing)
            .into_iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["src", ".git"]);
    }

    // ---- defaults + traits summary ----

    #[test]
    fn clamp_reasoning_falls_to_the_model_default() {
        let ladder = [ReasoningLevel::Low, ReasoningLevel::High];
        assert_eq!(
            clamp_reasoning(Some(ReasoningLevel::High), &ladder),
            Some(ReasoningLevel::High)
        );
        // A foreign level never lingers.
        assert_eq!(
            clamp_reasoning(Some(ReasoningLevel::Ultra), &ladder),
            Some(ReasoningLevel::High)
        );
        assert_eq!(clamp_reasoning(None, &ladder), Some(ReasoningLevel::High));
        assert_eq!(clamp_reasoning(Some(ReasoningLevel::High), &[]), None);
    }

    #[test]
    fn default_reasoning_falls_to_medium_then_the_first_entry() {
        assert_eq!(
            default_reasoning(&[ReasoningLevel::Minimal]),
            Some(ReasoningLevel::Minimal)
        );
        assert_eq!(
            default_reasoning(&[ReasoningLevel::Low, ReasoningLevel::Medium]),
            Some(ReasoningLevel::Medium)
        );
    }

    #[test]
    fn default_model_is_the_first_catalog_row() {
        let models = vec![model("a/flagship", "Flagship"), model("a/cheap", "Cheap")];
        assert_eq!(
            default_model(&models).map(|m| m.id.as_str()),
            Some("a/flagship")
        );
        assert_eq!(default_model(&[]), None);
    }

    #[test]
    fn traits_summary_names_effective_choices() {
        let option = holt_proto::ModelOption {
            id: "speed".into(),
            label: "Speed".into(),
            choices: vec![
                holt_proto::ModelOptionChoice {
                    id: "balanced".into(),
                    label: "Balanced".into(),
                },
                holt_proto::ModelOptionChoice {
                    id: "fast".into(),
                    label: "Fast".into(),
                },
            ],
            default_choice: "balanced".into(),
        };
        let mut with_options = model("a/m", "M");
        with_options.options = vec![option];
        // Defaults are spelled out, joined with " · ".
        assert_eq!(
            traits_summary(
                Some(&with_options),
                Some(ReasoningLevel::High),
                &serde_json::Map::new()
            ),
            Some("High · Balanced".to_string())
        );
        // An explicit pick that is still offered wins.
        let mut picks = serde_json::Map::new();
        picks.insert(
            "speed".to_string(),
            serde_json::Value::String("fast".into()),
        );
        assert_eq!(
            traits_summary(Some(&with_options), Some(ReasoningLevel::High), &picks),
            Some("High · Fast".to_string())
        );
        // A stale pick (choice no longer offered) falls back to the default.
        let mut stale = serde_json::Map::new();
        stale.insert(
            "speed".to_string(),
            serde_json::Value::String("turbo".into()),
        );
        assert_eq!(
            traits_summary(Some(&with_options), Some(ReasoningLevel::High), &stale),
            Some("High · Balanced".to_string())
        );
        // Nothing to describe.
        assert_eq!(traits_summary(None, None, &serde_json::Map::new()), None);
        assert_eq!(
            traits_summary(Some(&model("a/m", "M")), None, &serde_json::Map::new()),
            None
        );
        // Reasoning alone still summarizes.
        assert_eq!(
            traits_summary(
                Some(&model("a/m", "M")),
                Some(ReasoningLevel::Max),
                &serde_json::Map::new()
            ),
            Some("Max".to_string())
        );
    }

    #[test]
    fn traits_customized_only_for_non_default_picks() {
        let option = holt_proto::ModelOption {
            id: "speed".into(),
            label: "Speed".into(),
            choices: vec![
                holt_proto::ModelOptionChoice {
                    id: "balanced".into(),
                    label: "Balanced".into(),
                },
                holt_proto::ModelOptionChoice {
                    id: "fast".into(),
                    label: "Fast".into(),
                },
            ],
            default_choice: "balanced".into(),
        };
        let mut with_options = model("a/m", "M");
        with_options.options = vec![option];
        let ladder = [ReasoningLevel::High];
        // All defaults: quiet.
        assert!(!traits_customized(
            Some(&with_options),
            Some(ReasoningLevel::High),
            &ladder,
            &serde_json::Map::new()
        ));
        // A non-default option pick brightens the trigger.
        let mut picks = serde_json::Map::new();
        picks.insert(
            "speed".to_string(),
            serde_json::Value::String("fast".into()),
        );
        assert!(traits_customized(
            Some(&with_options),
            Some(ReasoningLevel::High),
            &ladder,
            &picks
        ));
        // A non-default reasoning level brightens it too.
        assert!(traits_customized(
            Some(&with_options),
            Some(ReasoningLevel::Max),
            &ladder,
            &serde_json::Map::new()
        ));
        // A stale pick that is no longer offered does not.
        let mut stale = serde_json::Map::new();
        stale.insert(
            "speed".to_string(),
            serde_json::Value::String("turbo".into()),
        );
        assert!(!traits_customized(
            Some(&with_options),
            Some(ReasoningLevel::High),
            &ladder,
            &stale
        ));
    }

    #[test]
    fn reasoning_label_spells_out_every_level() {
        assert_eq!(reasoning_label(ReasoningLevel::High), "High");
        assert_eq!(reasoning_label(ReasoningLevel::XHigh), "X-High");
        assert_eq!(reasoning_label(ReasoningLevel::Ultrathink), "Ultrathink");
    }
}
