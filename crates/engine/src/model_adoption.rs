//! What a vendor listing's models become under a provider
//! (ADR-fetched-models-adopt-the-catalogs-record). A provider the catalog
//! does not know — a company gateway, a relay — answers a model listing
//! with bare ids, and those ids are usually models the catalog carries
//! elsewhere. The row filed for such an id takes the catalog's record for
//! it: the maker's own listing (`modelVendors`), ahead of a host's copy of
//! the model or a gateway's row for it.
//!
//! What carries over depends on where the record stands and whether the
//! provider speaks its dialect. A maker's record in the same dialect is
//! taken whole — the ladder's wire values and `compat` are the model's own
//! protocol, which is what a relay forwards to. Anything else, and any
//! other dialect, takes the facts and the dialect's default encoding:
//! whether the model reasons, which levels it has and which it lacks, the
//! windows, the modalities, the price. A host's or a gateway's switches say
//! how to talk to that host, not to the model.
//!
//! Where the catalog falls short it estimates, and says so. A record that
//! reasons but names no levels takes the levels of the nearest sibling in
//! its maker's listings. An id the catalog does not carry at all starts
//! from the nearest sibling it can see — one already on the provider, or
//! one the same listing adopts — taking that row's numbers and, when the
//! row is the provider's own in the same dialect, the protocol it speaks;
//! with no sibling, from the plain defaults every blank record used to get.
//! Either estimate is the user's to correct before anything is saved.
//!
//! An adopted record is whole, so it always serves; and it carries a digest
//! of the catalog-supplied fields, so an upgrade can tell an adoption nobody
//! touched from one somebody edited. The first is regenerated from the
//! catalog as it stands now — a refresh reaches it — keeping its own id,
//! name, dialect, and endpoint. The second is the user's and applies as
//! written, as does a record whose source the catalog no longer carries:
//! its last adoption is then the best known.

use std::{collections::HashMap, fmt::Write as _};

use holt_core::ai::{
    models::get_supported_thinking_levels,
    models_generated::model_vendors,
    types::{Model as CoreModel, ModelInput, ModelThinkingLevel, ThinkingLevelMap},
};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::provider_store::{Catalog, canonical_json};

/// `extra` keys of an adopted record: where its facts came from, and the
/// digest of them as adopted.
pub(crate) const ADOPTED_FROM: &str = "adoptedFrom";
pub(crate) const ADOPTED_DIGEST: &str = "adoptedDigest";

/// The fields adoption fills from the catalog — and the only ones the
/// digest covers. The id, name, dialect, and endpoint are the row's own.
const ADOPTED_FIELDS: [&str; 9] = [
    "reasoning",
    "thinkingLevelMap",
    "input",
    "cost",
    "contextWindow",
    "maxTokens",
    "compat",
    "samplingParams",
    "headers",
];

/// The levels a picker can offer, in ladder order.
pub(crate) const LEVELS: [ModelThinkingLevel; 6] = [
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// What a new model's numbers are when nothing nearer is known — the values
/// the fetch dialog's blank record always carried.
pub(crate) const PLAIN_CONTEXT_WINDOW: u64 = 200_000;
pub(crate) const PLAIN_MAX_TOKENS: u64 = 8_192;

/// An id as a listing offers it, with whatever names the vendor in front
/// taken off: `openai/gpt-5.4` is `gpt-5.4`.
pub(crate) fn bare_id(model_id: &str) -> &str {
    model_id.rsplit('/').next().unwrap_or(model_id)
}

/// An id as the model list shows names (`GLM-5.2 Highspeed`, `Claude Opus
/// 4.7`): a known family keeps its acronym and its version hangs off it,
/// later words capitalize, a second number continues the first (`4-7` reads
/// `4.7`), and a trailing date goes in brackets. An id outside every family
/// keeps itself with its first letter raised.
pub fn display_name(model_id: &str) -> String {
    const FAMILIES: [(&str, &str); 13] = [
        ("gpt", "GPT"),
        ("glm", "GLM"),
        ("kimi", "Kimi"),
        ("claude", "Claude"),
        ("deepseek", "DeepSeek"),
        ("gemini", "Gemini"),
        ("gemma", "Gemma"),
        ("qwen", "Qwen"),
        ("minimax", "MiniMax"),
        ("mimo", "MiMo"),
        ("llama", "Llama"),
        ("grok", "Grok"),
        ("mistral", "Mistral"),
    ];
    let family_of = |token: &str| {
        FAMILIES
            .iter()
            .find(|(key, _)| *key == token)
            .map(|(_, name)| (*name, ""))
    };
    let bare = bare_id(model_id);
    let (core, date) = split_date(bare);
    let mut tokens = core.split('-');
    let first = tokens.next().unwrap_or_default();
    // `qwen3-max`: the family with its version glued on.
    let glued = family_of(first).or_else(|| {
        FAMILIES.iter().find_map(|(key, name)| {
            first
                .strip_prefix(key)
                .filter(|rest| starts_version(rest))
                .map(|rest| (*name, rest))
        })
    });
    let Some((acronym, glued_version)) = glued else {
        return capitalize(bare);
    };
    let mut name = format!("{acronym}{glued_version}");
    let mut after_family = glued_version.is_empty();
    let mut on_version = false;
    for token in tokens {
        if let Some((word, _)) = family_of(token) {
            name.push(' ');
            name.push_str(word);
            after_family = false;
            on_version = false;
        } else if after_family && starts_version(token) && token.starts_with('v') {
            name.push_str(&format!(" {}", token.to_uppercase()));
            after_family = false;
            on_version = false;
        } else if token.starts_with(|character: char| character.is_ascii_digit()) {
            if after_family {
                name.push('-');
            } else if on_version && is_number(token) {
                name.push('.');
            } else {
                name.push(' ');
            }
            name.push_str(token);
            after_family = false;
            on_version = true;
        } else {
            name.push(' ');
            name.push_str(&capitalize(token));
            after_family = false;
            on_version = false;
        }
    }
    if let Some(date) = date {
        name.push_str(&format!(" ({date})"));
    }
    name
}

/// Whether the token opens a version: a digit, or `v` followed by one.
fn starts_version(token: &str) -> bool {
    token.starts_with(|character: char| character.is_ascii_digit())
        || (token.starts_with('v')
            && token[1..].starts_with(|character: char| character.is_ascii_digit()))
}

fn is_number(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
}

fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// The id without its trailing date, and that date as `YYYY-MM-DD`: a dated
/// snapshot is the moving alias's family, not its own.
fn split_date(bare: &str) -> (&str, Option<String>) {
    let bytes = bare.as_bytes();
    let digits = |from: usize, to: usize| bytes[from..to].iter().all(u8::is_ascii_digit);
    if let Some(at) = bytes.len().checked_sub(11).filter(|at| {
        bytes[*at] == b'-'
            && digits(at + 1, at + 5)
            && bytes[at + 5] == b'-'
            && digits(at + 6, at + 8)
            && bytes[at + 8] == b'-'
            && digits(at + 9, at + 11)
    }) {
        return (&bare[..at], Some(bare[at + 1..].to_string()));
    }
    if let Some(at) = bytes
        .len()
        .checked_sub(9)
        .filter(|at| bytes[*at] == b'-' && digits(at + 1, at + 9))
    {
        let date = &bare[at + 1..];
        return (
            &bare[..at],
            Some(format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8])),
        );
    }
    (bare, None)
}

/// The levels a record offers, as the picker lists them: `off` has no tier
/// of its own.
pub(crate) fn levels(model: &CoreModel) -> Vec<String> {
    get_supported_thinking_levels(model)
        .into_iter()
        .filter(|level| *level != ModelThinkingLevel::Off)
        .map(level_name)
        .collect()
}

/// A level as the wire and the `ListModels` ladder name it.
pub(crate) fn level_name(level: ModelThinkingLevel) -> String {
    match level {
        ModelThinkingLevel::Off => "none".to_string(),
        ModelThinkingLevel::Minimal => "minimal".to_string(),
        ModelThinkingLevel::Low => "low".to_string(),
        ModelThinkingLevel::Medium => "medium".to_string(),
        ModelThinkingLevel::High => "high".to_string(),
        ModelThinkingLevel::Xhigh => "xhigh".to_string(),
        ModelThinkingLevel::Max => "max".to_string(),
    }
}

/// One row of a fetched listing as the engine would file it.
pub struct FetchedRow {
    pub id: String,
    /// The `provider/model` the catalog holds for this id; `None` for an id
    /// it does not carry.
    pub source: Option<String>,
    /// For an id the catalog does not carry: the `provider/model` it starts
    /// from — a sibling already on the provider or in this listing — or
    /// `None` for the plain defaults.
    pub starts_from: Option<String>,
    /// The levels are an estimate, not a record's own: the catalog names
    /// none for the model, so the row takes its family's.
    pub levels_estimated: bool,
    /// The `provider/model` the estimate came from; `None` when no sibling
    /// names levels and the usual four stand in.
    pub estimated_from: Option<String>,
    pub model: CoreModel,
}

/// One row of a fetched listing as the dialog saves it.
pub struct FetchedSave {
    pub id: String,
    /// Blank keeps the default — the catalog's name, or the id's own.
    pub name: String,
    /// The levels the user chose. `None` leaves an adoption's own in place;
    /// an id the catalog does not carry takes the list as given, empty too.
    pub levels: Option<Vec<String>>,
    pub can_turn_off: bool,
    /// Required for an id the catalog does not carry; ignored otherwise.
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    /// `None` keeps what the row already carries.
    pub cost: Option<holt_core::ai::types::ModelCost>,
    pub image: Option<bool>,
    /// The `provider/model` a new row starts from, as the preview named it.
    pub starts_from: Option<String>,
}

/// Where a carrier stands in the order the rule prefers it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank<'a> {
    /// A maker's listing, then a carrier with a list price (a subscription's
    /// zero is no price), then any carrier.
    tier: u8,
    /// An id with the maker named in front of it — `openai/gpt-5.4`,
    /// `@cf/zai-org/glm-5.3` — is a listing that serves someone's model.
    prefixed: bool,
    provider: &'a str,
}

/// Every model the catalog can adopt, keyed by bare id.
pub(crate) struct AdoptionIndex {
    sources: HashMap<String, CoreModel>,
}

impl AdoptionIndex {
    /// One record per bare id. Built once per boot over the merged catalog,
    /// so an override in the overlay is what gets adopted.
    pub(crate) fn build(catalog: &Catalog) -> Self {
        let mut sources: HashMap<&str, (Rank, &CoreModel)> = HashMap::new();
        for provider in catalog.iter() {
            for model in &provider.models {
                let rank = Rank {
                    tier: match (
                        is_maker(&provider.id, &model.id),
                        model.cost.rates.input.0 > 0.0,
                    ) {
                        (true, _) => 0,
                        (false, true) => 1,
                        (false, false) => 2,
                    },
                    prefixed: model.id.contains('/'),
                    provider: provider.id.as_str(),
                };
                let slot = sources
                    .entry(bare_id(&model.id))
                    .or_insert_with(|| (rank, model));
                if rank < slot.0 {
                    *slot = (rank, model);
                }
            }
        }
        Self {
            sources: sources
                .into_iter()
                .map(|(bare, (_, model))| (bare.to_string(), model.clone()))
                .collect(),
        }
    }

    /// The record to adopt for a listing's id; `None` for an id the catalog
    /// does not carry.
    pub(crate) fn source(&self, model_id: &str) -> Option<&CoreModel> {
        self.sources.get(bare_id(model_id))
    }
}

/// Whether the provider's listing is where this model lives: a maker's
/// listing is its own throughout, and a maker's plan only under its family's
/// names — the Qwen token plans serve GLM and Kimi beside Qwen.
fn is_maker(provider_id: &str, model_id: &str) -> bool {
    match model_vendors().get(provider_id) {
        Some(prefixes) if prefixes.is_empty() => true,
        Some(prefixes) => {
            let bare = bare_id(model_id).to_ascii_lowercase();
            prefixes.iter().any(|prefix| bare.starts_with(prefix))
        }
        None => false,
    }
}

/// The id's family: the lowercase run before its first digit, so `glm-4.5`,
/// `GLM-5.1` and `glm-5` are one family and `gpt-oss-120b` is not `gpt-5.4`'s.
fn family(bare: &str) -> String {
    let lower = bare.to_lowercase();
    match lower.find(|character: char| character.is_ascii_digit()) {
        Some(at) => lower[..at].to_string(),
        None => lower,
    }
}

/// The id's version: the first number in it as major and minor, a hyphen
/// between digits read as the decimal point — `4-7` is 4.7. Whole numbers, so
/// two versions equally far stay a tie.
fn version(bare: &str) -> Option<(u64, u64)> {
    let start = bare.find(|character: char| character.is_ascii_digit())?;
    let mut major = String::new();
    let mut minor = String::new();
    let mut rest = bare[start..].chars().peekable();
    let mut on_minor = false;
    while let Some(character) = rest.next() {
        if character.is_ascii_digit() {
            if on_minor {
                minor.push(character);
            } else {
                major.push(character);
            }
            continue;
        }
        let decimal = !on_minor
            && (character == '.'
                || character == '-' && rest.peek().is_some_and(char::is_ascii_digit));
        if !decimal {
            break;
        }
        on_minor = true;
    }
    let minor = if minor.is_empty() {
        "0".to_string()
    } else {
        minor
    };
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The nearest sibling of an id among candidates: its own family, then the
/// closest version, the first listed winning a tie.
pub(crate) fn nearest_sibling<'a>(
    model_id: &str,
    candidates: impl IntoIterator<Item = &'a CoreModel>,
) -> Option<&'a CoreModel> {
    let bare = bare_id(model_id);
    let (own_family, own_version) = (family(bare), version(bare));
    let mut nearest: Option<((u64, u64), &CoreModel)> = None;
    for candidate in candidates {
        let candidate_bare = bare_id(&candidate.id);
        if family(candidate_bare) != own_family || candidate_bare == bare {
            continue;
        }
        let distance = match (own_version, version(candidate_bare)) {
            (Some((major, minor)), Some((other_major, other_minor))) => {
                (major.abs_diff(other_major), minor.abs_diff(other_minor))
            }
            _ => (u64::MAX, u64::MAX),
        };
        if nearest.is_none_or(|(closest, _)| distance < closest) {
            nearest = Some((distance, candidate));
        }
    }
    nearest.map(|(_, candidate)| candidate)
}

/// Whether the record itself names its levels — a ladder is written into it,
/// not the default one a reasoning record with no ladder is served with.
pub(crate) fn names_levels(model: &CoreModel) -> bool {
    model
        .thinking_level_map
        .as_ref()
        .is_some_and(|map| map.values().any(|value| value.is_some()))
}

/// The ladder every blank reasoning record offers, as the dialog's new rows
/// start when nothing nearer is known.
pub(crate) fn default_levels() -> Vec<String> {
    ["minimal", "low", "medium", "high"]
        .iter()
        .map(|level| level.to_string())
        .collect()
}

fn can_turn_off(model: &CoreModel) -> bool {
    get_supported_thinking_levels(model).contains(&ModelThinkingLevel::Off)
}

fn origin(model: &CoreModel) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// What a record that reasons but names no levels gets: the levels of the
/// nearest sibling in a maker's listings that names them, and where they came
/// from. `None` when the record needs no estimate.
pub(crate) fn estimated_levels(
    index: &AdoptionIndex,
    source: &CoreModel,
) -> Option<(Vec<String>, String)> {
    if !source.reasoning || names_levels(source) {
        return None;
    }
    // The index is a hash map, so the candidates are put in a stable order
    // first: a tie must fall the same way on every boot.
    let mut siblings: Vec<&CoreModel> = index
        .sources
        .values()
        .filter(|model| is_maker(&model.provider, &model.id) && names_levels(model))
        .collect();
    siblings.sort_by_key(|model| (model.provider.clone(), model.id.clone()));
    nearest_sibling(&source.id, siblings).map(|sibling| (levels(sibling), origin(sibling)))
}

/// The row adoption files under `provider_id` for a listing's `model_id`:
/// the catalog's record for it, retargeted to the provider's endpoint and
/// dialect and marked as adopted. A record that names no levels arrives with
/// its family's, so the estimate rides with every later regeneration.
pub(crate) fn adopt(
    index: &AdoptionIndex,
    source: &CoreModel,
    provider_id: &str,
    model_id: &str,
    dialect: &str,
    base_url: &str,
) -> CoreModel {
    // Only the maker's own protocol switches are worth carrying: a gateway's
    // or a host's say how to talk to that gateway, not to the model.
    let mut model = if is_maker(&source.provider, &source.id) && source.api == dialect {
        let mut same = source.clone();
        // Routing inside one gateway is not the vendor's protocol, and the
        // row is no longer behind that gateway.
        if let Some(compat) = same.compat.as_mut() {
            compat.open_router_routing = None;
            compat.vercel_gateway_routing = None;
        }
        same
    } else {
        CoreModel {
            id: String::new(),
            name: source.name.clone(),
            api: dialect.to_string(),
            provider: String::new(),
            base_url: String::new(),
            reasoning: source.reasoning,
            thinking_level_map: level_set(source.thinking_level_map.as_ref()),
            input: source.input.clone(),
            cost: source.cost.clone(),
            context_window: source.context_window,
            max_tokens: source.max_tokens,
            sampling_params: None,
            headers: None,
            compat: None,
            extra: Default::default(),
        }
    };
    model.id = model_id.to_string();
    model.provider = provider_id.to_string();
    model.base_url = base_url.to_string();
    if let Some((estimated, _)) = estimated_levels(index, source) {
        set_levels(&mut model, &estimated, can_turn_off(source))
            .expect("a catalog ladder names only known levels");
    }
    model.extra.insert(
        ADOPTED_FROM.to_string(),
        json!(format!("{}/{}", source.provider, source.id)),
    );
    model
        .extra
        .insert(ADOPTED_DIGEST.to_string(), json!(adoption_digest(&model)));
    model
}

/// The record a new id starts from: a sibling's numbers and — when the
/// sibling is the provider's own row in this dialect — the protocol that row
/// speaks. Nothing of the sibling's identity, price, or markers carries over.
pub(crate) fn start_from(
    sibling: &CoreModel,
    provider_id: &str,
    model_id: &str,
    dialect: &str,
    base_url: &str,
) -> CoreModel {
    let mut model = if sibling.provider == provider_id && sibling.api == dialect {
        let mut same = sibling.clone();
        if let Some(compat) = same.compat.as_mut() {
            compat.open_router_routing = None;
            compat.vercel_gateway_routing = None;
        }
        same
    } else {
        CoreModel {
            id: String::new(),
            name: String::new(),
            api: dialect.to_string(),
            provider: String::new(),
            base_url: String::new(),
            reasoning: sibling.reasoning,
            thinking_level_map: level_set(sibling.thinking_level_map.as_ref()),
            input: sibling.input.clone(),
            cost: Default::default(),
            context_window: sibling.context_window,
            max_tokens: sibling.max_tokens,
            sampling_params: None,
            headers: None,
            compat: None,
            extra: Default::default(),
        }
    };
    model.id = model_id.to_string();
    model.name = display_name(model_id);
    model.provider = provider_id.to_string();
    model.base_url = base_url.to_string();
    // A price is the model's own, never its sibling's.
    model.cost = Default::default();
    model.extra.clear();
    model
}

/// A new id's record when no sibling is in sight: the id under the
/// provider's endpoint and dialect, with the plain defaults.
pub(crate) fn plain_row(
    provider_id: &str,
    model_id: &str,
    dialect: &str,
    base_url: &str,
) -> CoreModel {
    CoreModel {
        id: model_id.to_string(),
        name: display_name(model_id),
        api: dialect.to_string(),
        provider: provider_id.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: Default::default(),
        context_window: PLAIN_CONTEXT_WINDOW,
        max_tokens: PLAIN_MAX_TOKENS,
        sampling_params: None,
        headers: None,
        compat: None,
        extra: Default::default(),
    }
}

/// Which levels the model has, freed of how another dialect wrote them: a
/// level the source marks unsupported stays unsupported, a supported one
/// takes the dialect's own value for it, and `off` is left to the dialect
/// unless the model cannot be turned off.
fn level_set(map: Option<&ThinkingLevelMap>) -> Option<ThinkingLevelMap> {
    let map = map?;
    let mut set = ThinkingLevelMap::new();
    for (level, value) in map {
        let entry = match (level, value) {
            (ModelThinkingLevel::Off, None) => Some(None),
            (ModelThinkingLevel::Off, Some(_)) => None,
            (_, None) => Some(None),
            (_, Some(_)) => Some(Some(level_name(*level))),
        };
        if let Some(entry) = entry {
            set.insert(*level, entry);
        }
    }
    Some(set)
}

/// Sets which levels the record offers: each of the six is offered or marked
/// unsupported, and `off` is withdrawn when the model cannot be turned off
/// (a wire value it had for `off` is kept). A record with no level does not
/// reason.
pub(crate) fn set_levels(
    record: &mut CoreModel,
    chosen: &[String],
    can_turn_off: bool,
) -> Result<(), String> {
    for name in chosen {
        if !LEVELS.iter().any(|level| &level_name(*level) == name) {
            return Err(format!("{name} is not a thinking level"));
        }
    }
    let off = record
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&ModelThinkingLevel::Off))
        .cloned()
        .flatten();
    let mut map = ThinkingLevelMap::new();
    for level in LEVELS {
        let name = level_name(level);
        map.insert(level, chosen.contains(&name).then_some(name));
    }
    if !can_turn_off {
        map.insert(ModelThinkingLevel::Off, None);
    } else if let Some(off) = off {
        map.insert(ModelThinkingLevel::Off, Some(off));
    }
    record.thinking_level_map = Some(map);
    record.reasoning = !chosen.is_empty();
    Ok(())
}

/// Lowercase hex SHA-256 of the catalog-supplied fields as stored. A record
/// still matching it is the adoption's own; one that does not was edited by
/// hand.
pub(crate) fn adoption_digest(model: &CoreModel) -> String {
    let Ok(serde_json::Value::Object(object)) = serde_json::to_value(model) else {
        return String::new();
    };
    let covered = object
        .into_iter()
        .filter(|(key, _)| ADOPTED_FIELDS.contains(&key.as_str()))
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let canonical = canonical_json(&serde_json::Value::Object(covered)).to_string();
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(canonical.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The record as the engine serves it: an adoption nobody edited since is
/// regenerated from the catalog as it stands now. `None` for anything else
/// — no adoption, an edited one, or one whose source the catalog no longer
/// carries — and the stored record then applies as written.
pub(crate) fn following_catalog(
    index: &AdoptionIndex,
    provider_id: &str,
    stored: &CoreModel,
) -> Option<CoreModel> {
    let digest = stored.extra.get(ADOPTED_DIGEST)?.as_str()?;
    if digest.is_empty() || adoption_digest(stored) != digest {
        return None;
    }
    let source = index.source(&stored.id)?;
    let mut fresh = adopt(
        index,
        source,
        provider_id,
        &stored.id,
        &stored.api,
        &stored.base_url,
    );
    fresh.name = stored.name.clone();
    Some(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_core::ai::models::get_supported_thinking_levels;

    /// The compiled catalog as boot merges it from an empty data dir.
    fn index() -> AdoptionIndex {
        let dir = tempfile::tempdir().unwrap();
        AdoptionIndex::build(&crate::provider_store::load(dir.path()))
    }

    /// A servable record with nothing but an id, a ladder when given one,
    /// and — so protocol can be seen carrying or not — a marker in `compat`.
    fn model(provider: &str, id: &str, ladder: Option<&[&str]>) -> CoreModel {
        let mut record: CoreModel = serde_json::from_value(json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": "https://example.invalid",
            "reasoning": ladder.is_some(),
            "input": ["text"],
            "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.0, "cacheWrite": 0.0},
            "contextWindow": 1000,
            "maxTokens": 100,
            "compat": {"supportsStore": false},
        }))
        .unwrap();
        if let Some(ladder) = ladder {
            set_levels(&mut record, &names(ladder), true).unwrap();
        }
        record
    }

    fn names(ladder: &[&str]) -> Vec<String> {
        ladder.iter().map(|level| level.to_string()).collect()
    }

    fn index_of(models: &[CoreModel]) -> AdoptionIndex {
        AdoptionIndex {
            sources: models
                .iter()
                .map(|model| (bare_id(&model.id).to_string(), model.clone()))
                .collect(),
        }
    }

    fn offers_off(model: &CoreModel) -> bool {
        get_supported_thinking_levels(model).contains(&ModelThinkingLevel::Off)
    }

    #[test]
    fn display_names_follow_the_model_list_s_rule() {
        for (id, name) in [
            ("gpt-5.4", "GPT-5.4"),
            ("gpt-4o", "GPT-4o"),
            ("gpt-4.1-mini", "GPT-4.1 Mini"),
            ("glm-4.5-air", "GLM-4.5 Air"),
            ("glm-5.2-highspeed", "GLM-5.2 Highspeed"),
            ("openai/claude-opus-4-7", "Claude Opus 4.7"),
            ("claude-haiku-4-5-20251101", "Claude Haiku 4.5 (2025-11-01)"),
            ("gpt-5.4-2026-11-01", "GPT-5.4 (2026-11-01)"),
            ("kimi-k3.1-pro", "Kimi K3.1 Pro"),
            ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("qwen3-max", "Qwen3 Max"),
            ("minimax-m2.5", "MiniMax M2.5"),
            ("acme-1", "Acme-1"),
            ("gw/acme_v2", "Acme_v2"),
        ] {
            assert_eq!(display_name(id), name, "{id}");
        }
    }

    #[test]
    fn the_maker_s_listing_is_the_record_not_a_gateway_s() {
        let index = index();

        let gpt = index.source("gpt-5.4").unwrap();
        assert_eq!(
            (gpt.provider.as_str(), gpt.id.as_str()),
            ("openai", "gpt-5.4")
        );
        assert_eq!(gpt.context_window, 1_050_000);
        let claude = index.source("anthropic/claude-opus-4-7").unwrap();
        assert_eq!(claude.provider, "anthropic");
        // Cloudflare's gateway row for the same model records two levels;
        // Anthropic's own records five, and none of them is `minimal`.
        assert_eq!(
            levels(claude),
            names(&["low", "medium", "high", "xhigh", "max"])
        );
        let glm = index.source("glm-5.3").unwrap();
        assert_eq!(glm.provider, "zai");
        assert_eq!(levels(glm), names(&["low", "high", "max"]));
    }

    #[test]
    fn a_host_s_copy_and_a_gateway_s_row_do_not_stand_in_for_the_maker() {
        let index = index();

        // Workers AI serves GLM-5.3 as `@cf/zai-org/glm-5.3` with a copy's
        // limits, earlier in the catalog than Z.AI's own row.
        let glm = index.source("glm-5.3").unwrap();
        assert_eq!(glm.max_tokens, 131_072);
        // Kimi K3.1 Pro: Moonshot's row, not its regional twin's or a
        // plan's.
        assert_eq!(
            index.source("kimi-k3.1-pro").unwrap().provider,
            "moonshotai"
        );
    }

    #[test]
    fn a_plan_is_the_maker_only_for_its_own_family() {
        let index = index();

        // The Qwen token plans serve GLM-5.3 beside Qwen, and sort ahead of
        // Z.AI: the model still comes from its maker.
        assert_eq!(index.source("glm-5.3").unwrap().provider, "zai");
        // GLM-5.2 left Z.AI's API listing and stays on its coding plan,
        // whose row speaks Z.AI's protocol — so it is taken whole.
        let source = index.source("glm-5.2").unwrap();
        assert_eq!(source.provider, "zai-coding-cn");
        assert!(source.compat.is_some());
        let model = adopt(
            &index,
            source,
            "gw",
            "glm-5.2",
            "openai-completions",
            "https://gw.example/v1",
        );
        assert_eq!(model.compat, source.compat);
        // A model the plans serve but do not make is not theirs.
        let deepseek = index.source("deepseek-v4-flash").unwrap();
        assert!(!deepseek.provider.starts_with("qwen"));
    }

    #[test]
    fn an_id_the_catalog_does_not_carry_has_no_source() {
        let index = index();

        assert!(index.source("acme-1").is_none());
        // GPT-4 left OpenAI's own row; a gateway still lists it, so a
        // listing that offers it adopts that row's facts.
        assert_ne!(index.source("gpt-4").unwrap().provider, "openai");
    }

    #[test]
    fn a_record_that_is_not_the_maker_s_gives_facts_but_not_its_switches() {
        let index = index();
        // Only plans and gateways carry this one; the first with a list
        // price speaks DeepSeek's thinking format to their own endpoint.
        let source = index.source("deepseek-v4-flash").unwrap();
        assert!(source.compat.is_some());

        let model = adopt(
            &index,
            source,
            "gw",
            "deepseek-v4-flash",
            &source.api,
            "https://gw.example/v1",
        );

        assert_eq!(model.compat, None);
        assert_eq!(model.headers, None);
        assert_eq!(model.context_window, source.context_window);
        assert_eq!(levels(&model), levels(source));
    }

    #[test]
    fn the_same_dialect_takes_the_record_whole() {
        let index = index();
        let source = index.source("glm-5.3").unwrap();

        let model = adopt(
            &index,
            source,
            "gw",
            "glm-5.3",
            "openai-completions",
            "https://gw.example/v1",
        );

        assert_eq!(model.provider, "gw");
        assert_eq!(model.base_url, "https://gw.example/v1");
        assert_eq!(model.name, source.name);
        assert_eq!(levels(&model), names(&["low", "high", "max"]));
        assert_eq!(model.compat, source.compat);
        assert_eq!(model.cost, source.cost);
        assert_eq!(model.max_tokens, source.max_tokens);
        assert_eq!(model.extra[ADOPTED_FROM], "zai/glm-5.3");
    }

    #[test]
    fn another_dialect_takes_the_facts_and_its_own_encoding() {
        let index = index();
        let source = index.source("gpt-5.4").unwrap();
        assert_eq!(source.api, "openai-responses");

        let model = adopt(
            &index,
            source,
            "gw",
            "gpt-5.4",
            "openai-completions",
            "https://gw.example/v1",
        );

        assert_eq!(model.api, "openai-completions");
        assert_eq!(levels(&model), names(&["low", "medium", "high", "xhigh"]));
        assert_eq!(model.context_window, 1_050_000);
        assert_eq!(model.cost, source.cost);
        assert_eq!(model.input, source.input);
        // The Responses dialect's switches say nothing about completions.
        assert_eq!(model.compat, None);
        // Each level is offered under the dialect's own value, and the two
        // the model lacks are marked so the picker never offers them.
        let map = model.thinking_level_map.as_ref().unwrap();
        assert_eq!(map[&ModelThinkingLevel::Xhigh].as_deref(), Some("xhigh"));
        assert_eq!(map[&ModelThinkingLevel::Minimal], None);
        assert_eq!(map[&ModelThinkingLevel::Max], None);
        assert!(!map.contains_key(&ModelThinkingLevel::Off));
    }

    #[test]
    fn a_model_that_cannot_stop_thinking_keeps_that_in_any_dialect() {
        let index = index();
        let source = index.source("glm-5.3").unwrap();

        let model = adopt(
            &index,
            source,
            "gw",
            "glm-5.3",
            "anthropic-messages",
            "https://gw.example/v1",
        );

        assert!(!offers_off(&model));
        assert_eq!(levels(&model), names(&["low", "high", "max"]));
    }

    #[test]
    fn a_record_that_names_no_levels_takes_its_family_s() {
        let index = index();

        // Z.AI's coding plan records no ladder for GLM-5.1; GLM-5.2 on the
        // same plan is its nearest sibling that names levels, and those ride
        // along — kept when the adoption is regenerated, and the dialog is
        // told they are an estimate.
        let source = index.source("glm-5.1").unwrap();
        assert!(source.thinking_level_map.is_none());
        assert_eq!(
            estimated_levels(&index, source),
            Some((names(&["high", "max"]), "zai-coding-cn/glm-5.2".to_string()))
        );
        let stored = adopt(
            &index,
            source,
            "gw",
            "glm-5.1",
            "openai-completions",
            "https://gw.example/v1",
        );
        assert_eq!(levels(&stored), names(&["high", "max"]));
        assert_eq!(
            levels(&following_catalog(&index, "gw", &stored).unwrap()),
            names(&["high", "max"])
        );

        // A record that names its own levels is never estimated.
        assert!(estimated_levels(&index, index.source("glm-5.3").unwrap()).is_none());
        // Nor one that does not reason.
        let mute = model("acme", "acme-1", None);
        assert!(estimated_levels(&index_of(std::slice::from_ref(&mute)), &mute).is_none());
    }

    #[test]
    fn the_nearest_sibling_is_the_closest_version_of_the_same_family() {
        let listed = [
            model("p", "glm-4.6", Some(&["low", "high"])),
            model("p", "glm-5.1", Some(&["low", "max"])),
            model("p", "glm-5.3", Some(&["low", "max"])),
            model("p", "gpt-5.3", Some(&["low"])),
        ];
        let nearest = |id: &str| nearest_sibling(id, listed.iter()).map(|m| m.id.as_str());

        assert_eq!(nearest("glm-4.5"), Some("glm-4.6"));
        assert_eq!(nearest("GLM-5.2"), Some("glm-5.1"));
        assert_eq!(nearest("gw/glm-5.2"), Some("glm-5.1"));
        // A tie goes to the first listed.
        assert_eq!(nearest("glm-5.2-highspeed"), Some("glm-5.1"));
        // Another family is no sibling, and neither is the id itself.
        assert_eq!(nearest("gpt-oss-120b"), None);
        assert_eq!(nearest("glm-5.3"), Some("glm-5.1"));
        assert_eq!(nearest("acme-1"), None);
    }

    #[test]
    fn a_new_row_starts_from_its_sibling_s_numbers_and_its_provider_s_protocol() {
        let sibling = model("zai-coding-cn", "glm-4.7", Some(&["low", "high"]));
        let mut sibling = sibling;
        sibling.context_window = 204_800;
        sibling.max_tokens = 131_072;
        sibling
            .extra
            .insert(ADOPTED_DIGEST.to_string(), json!("marker"));

        let same = start_from(
            &sibling,
            "zai-coding-cn",
            "glm-4.5",
            "openai-completions",
            "https://api.z.ai/coding/paas/v4",
        );

        assert_eq!(same.name, "GLM-4.5");
        assert_eq!(same.provider, "zai-coding-cn");
        assert_eq!(same.base_url, "https://api.z.ai/coding/paas/v4");
        assert_eq!((same.context_window, same.max_tokens), (204_800, 131_072));
        assert_eq!(levels(&same), names(&["low", "high"]));
        // The provider's own row speaks this model's protocol, so it comes
        // along; the sibling's price and markers do not.
        assert_eq!(same.compat, sibling.compat);
        assert_eq!(same.cost.rates.input.0, 0.0);
        assert!(same.extra.is_empty());

        // Under another dialect, or on another provider, only the facts.
        let elsewhere = start_from(
            &sibling,
            "gw",
            "glm-4.5",
            "anthropic-messages",
            "https://gw.example/v1",
        );
        assert_eq!(elsewhere.compat, None);
        assert_eq!(elsewhere.api, "anthropic-messages");
        assert_eq!(elsewhere.context_window, 204_800);
    }

    #[test]
    fn a_plain_row_carries_the_id_and_the_usual_defaults() {
        let row = plain_row(
            "gw",
            "gw/acme-1",
            "openai-completions",
            "https://gw.example/v1",
        );

        assert_eq!(row.name, "Acme-1");
        assert_eq!(row.provider, "gw");
        assert!(!row.reasoning);
        assert_eq!(row.context_window, 200_000);
        assert_eq!(row.max_tokens, 8_192);
        assert_eq!(row.cost.rates.input.0, 0.0);
    }

    #[test]
    fn setting_levels_offers_the_chosen_ones_and_marks_the_rest() {
        let mut record = plain_row(
            "gw",
            "acme-1",
            "openai-completions",
            "https://gw.example/v1",
        );
        set_levels(&mut record, &names(&["low", "max"]), false).unwrap();

        assert_eq!(levels(&record), names(&["low", "max"]));
        assert!(!offers_off(&record));
        let map = record.thinking_level_map.as_ref().unwrap();
        assert_eq!(map[&ModelThinkingLevel::Medium], None);

        // A wire value `off` had is kept when the model can be turned off.
        let mut off = plain_row("gw", "acme-1", "openai-responses", "https://gw.example/v1");
        off.thinking_level_map = Some(
            [(ModelThinkingLevel::Off, Some("none".to_string()))]
                .into_iter()
                .collect(),
        );
        set_levels(&mut off, &names(&["high"]), true).unwrap();
        assert_eq!(
            off.thinking_level_map.as_ref().unwrap()[&ModelThinkingLevel::Off],
            Some("none".to_string())
        );
        assert!(offers_off(&off));

        // No level at all: the model does not reason.
        set_levels(&mut record, &[], true).unwrap();
        assert!(!record.reasoning);
        assert!(levels(&record).is_empty());
        assert!(set_levels(&mut record, &["ultra".to_string()], true).is_err());
    }

    #[test]
    fn an_untouched_adoption_follows_the_catalog_and_an_edited_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = crate::provider_store::load(dir.path());
        let index = AdoptionIndex::build(&catalog);
        let stored = adopt(
            &index,
            index.source("gpt-5.4").unwrap(),
            "gw",
            "gpt-5.4",
            "openai-completions",
            "https://gw.example/v1",
        );

        // As adopted: regenerated, so it carries the catalog as it stands.
        let fresh = following_catalog(&index, "gw", &stored).unwrap();
        assert_eq!(fresh.context_window, stored.context_window);
        assert_eq!(fresh.extra[ADOPTED_DIGEST], stored.extra[ADOPTED_DIGEST]);
        assert_eq!(fresh.name, stored.name);

        // A renamed row still follows: the name is the user's, not a fact.
        let mut renamed = stored.clone();
        renamed.name = "Our GPT".to_string();
        let fresh = following_catalog(&index, "gw", &renamed).unwrap();
        assert_eq!(fresh.name, "Our GPT");

        // A hand-edited fact makes the record the user's, whole.
        let mut edited = stored.clone();
        edited.cost.rates.input = holt_core::ai::types::JsF64(1.0);
        assert!(following_catalog(&index, "gw", &edited).is_none());
    }

    #[test]
    fn an_adoption_whose_source_is_gone_applies_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let index = AdoptionIndex::build(&crate::provider_store::load(dir.path()));
        let stored = adopt(
            &index,
            index.source("gpt-5.4").unwrap(),
            "gw",
            "gpt-5.4",
            "openai-completions",
            "https://gw.example/v1",
        );
        // The next release retires the model: an index with nothing to
        // offer for the id.
        let empty = AdoptionIndex {
            sources: HashMap::new(),
        };

        assert!(following_catalog(&empty, "gw", &stored).is_none());
    }

    #[test]
    fn a_record_that_was_never_adopted_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let index = AdoptionIndex::build(&crate::provider_store::load(dir.path()));
        let mut plain = index.source("gpt-5.4").unwrap().clone();

        plain.extra.remove(ADOPTED_DIGEST);
        assert!(following_catalog(&index, "gw", &plain).is_none());
    }

    #[test]
    fn the_digest_covers_the_catalog_s_fields_and_nothing_else() {
        let index = index();
        let stored = adopt(
            &index,
            index.source("gpt-5.4").unwrap(),
            "gw",
            "gpt-5.4",
            "openai-completions",
            "https://gw.example/v1",
        );
        let digest = adoption_digest(&stored);
        let reborn =
            |moved: serde_json::Value| -> CoreModel { serde_json::from_value(moved).unwrap() };

        // The row's own: moving any of these leaves the digest alone.
        for (field, value) in [
            ("id", json!("gpt-5.4-alias")),
            ("name", json!("Renamed")),
            ("api", json!("anthropic-messages")),
            ("provider", json!("elsewhere")),
            ("baseUrl", json!("https://elsewhere.example/v1")),
            (ADOPTED_FROM, json!("openai/gpt-4")),
            (ADOPTED_DIGEST, json!("stale")),
        ] {
            let mut moved = serde_json::to_value(&stored).unwrap();
            moved[field] = value;
            assert_eq!(adoption_digest(&reborn(moved)), digest, "{field}");
        }

        // The catalog's: a different value under any of these is an edit.
        for (field, value) in [
            ("reasoning", json!(!stored.reasoning)),
            ("thinkingLevelMap", json!({ "off": "none" })),
            ("input", json!(["text"])),
            (
                "cost",
                json!({"input": 99.0, "output": 99.0, "cacheRead": 0.0, "cacheWrite": 0.0}),
            ),
            ("contextWindow", json!(stored.context_window + 1)),
            ("maxTokens", json!(stored.max_tokens + 1)),
            ("compat", json!({ "supportsStore": true })),
            ("samplingParams", json!({ "temperature": 0.5 })),
            ("headers", json!({ "X-Adoption-Test": "1" })),
        ] {
            let mut moved = serde_json::to_value(&stored).unwrap();
            moved[field] = value;
            assert_ne!(adoption_digest(&reborn(moved)), digest, "{field}");
        }
    }

    #[test]
    fn the_markers_ride_as_record_keys_outside_the_digest() {
        let index = index();
        let model = adopt(
            &index,
            index.source("gpt-5.4").unwrap(),
            "gw",
            "gpt-5.4",
            "openai-completions",
            "https://gw.example/v1",
        );

        // Flattened into the stored record, where only the adoption path
        // reads them: no transport builds a request from a model's `extra`.
        let value = serde_json::to_value(&model).unwrap();
        assert_eq!(value[ADOPTED_FROM], "openai/gpt-5.4");
        assert_eq!(value[ADOPTED_DIGEST].as_str().unwrap().len(), 64);
    }
}
