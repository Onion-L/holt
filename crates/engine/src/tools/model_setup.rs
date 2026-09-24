//! The provider-catalog tools (ADR-0029, now mounted by Provider Mode,
//! ADR-0037): the read-only `model_proposal` tool validates an exact
//! provider-catalog change and stores it engine-side; the only apply path
//! is the proposal card's `ApplyModelProposal` RPC, which executes a stored
//! proposal by id under the same revalidation — the agent never holds an
//! apply tool, so what the user saw in the transcript is bit-for-bit what
//! executes. API keys never enter this path: they live in the credential
//! store and surface only as a probe's Authorization header. Record header
//! values are the same kind of secret: the dump omits them and proposals can
//! neither read nor write them — a replacement keeps the stored headers.

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use futures::future::BoxFuture;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, Model as CoreModel, TextContent},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::ChatRuntime,
    provider_settings::{CustomProvider, ProviderSettingsSnapshot},
    provider_store,
    providers::ProviderAdapter,
};

/// Proposals kept per chat, newest last; older ones fall off.
const PROPOSAL_CAP: usize = 5;
/// A Write whose touched providers moved since the proposal was built.
const STALE_PROPOSAL: &str = "provider settings changed since this proposal was created; \
ask the assistant to propose again";
/// The `/models` probe's whole-request budget.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Probe listings are research aids, not catalogs: cap what rides back.
const PROBE_LISTING_CAP: usize = 200;

const PROPOSAL_DESCRIPTION: &str = "Prepare a provider-catalog change (add or update a model \
with full metadata, define a custom provider, hide dead models) WITHOUT writing anything. \
Validates the change against the local catalog, reports exactly what would change (a no-op \
says so), stores the result engine-side, and returns a proposalId; a proposal card with the \
diff appears in the conversation. Parameters: \
`changes` (an array; omit it to only inspect a provider), `providerId` (when \
inspecting: a concrete provider id; OMIT it entirely to list the organizations \
and their providers — resolve the user's words to one before proposing), `modelId` (when inspecting: dump that one model's complete record JSON — the \
template to copy when replacing it), `probe` (optional: live GET {baseUrl}/models against \
the provider using the stored key if one exists), `provider` (when inspecting a provider \
that is in neither the catalog nor a proposal yet: the draft {id, name, baseUrl, \
defaultApi} from its docs — the probe targets the draft's baseUrl, and the key rides only \
after the user saved one for exactly that baseUrl). Each change is an object with an `action` \
of: upsert_model_record ({providerId, record — a complete model record: id, name, api, \
provider, baseUrl, reasoning, input, cost, contextWindow, maxTokens, and optionally \
thinkingLevelMap/compat}), upsert_custom_provider ({provider: {id, name, baseUrl, \
defaultApi}}), remove_custom_provider ({providerId}), remove_model_record ({providerId, \
modelId}), or set_hidden_models ({providerId, modelIds}). When replacing an existing id, \
inspect it first and copy its api/compat/thinkingLevelMap, changing only what differs. \
Research model facts first with web_fetch/web_search, then propose. A newer proposal \
touching the same provider replaces the older one. After presenting the resulting diff, \
STOP — the user writes it from the proposal card; never claim to apply it yourself and \
never wait for an in-chat approval.";

// ---------------------------------------------------------------------------
// The change vocabulary
// ---------------------------------------------------------------------------

/// One exact catalog change. Stored verbatim in proposals and executed
/// verbatim on apply — this is the "as stored" of ADR-0029. Serialized in
/// the tool's own `action` vocabulary so a persisted proposal (ADR-0037)
/// reads like the call that made it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum CatalogChange {
    UpsertModelRecord {
        provider_id: String,
        record: Box<CoreModel>,
    },
    UpsertCustomProvider {
        provider: CustomProvider,
    },
    RemoveCustomProvider {
        provider_id: String,
    },
    RemoveModelRecord {
        provider_id: String,
        model_id: String,
    },
    SetHiddenModels {
        provider_id: String,
        model_ids: BTreeSet<String>,
    },
}

impl CatalogChange {
    pub(crate) fn provider_id(&self) -> &str {
        match self {
            CatalogChange::UpsertModelRecord { provider_id, .. }
            | CatalogChange::RemoveCustomProvider { provider_id }
            | CatalogChange::RemoveModelRecord { provider_id, .. }
            | CatalogChange::SetHiddenModels { provider_id, .. } => provider_id,
            CatalogChange::UpsertCustomProvider { provider } => &provider.id,
        }
    }
}

fn input_str<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("\"{key}\" is required"))
}

/// Parses one `changes` element. Everything the engine cannot serve is an
/// error here, never a silent drop — the model must fix its proposal.
pub(crate) fn parse_change(value: &serde_json::Value) -> Result<CatalogChange, String> {
    let action = input_str(value, "action")?;
    match action {
        "upsert_model_record" => {
            let provider_id = input_str(value, "providerId")?.to_string();
            let record = value
                .get("record")
                .cloned()
                .ok_or_else(|| "\"record\" is required".to_string())?;
            let record: CoreModel = serde_json::from_value(record)
                .map_err(|error| format!("\"record\" is not a complete model record: {error}"))?;
            if record.headers.is_some() {
                return Err(
                    "\"record\" must not set headers — header values are secret and \
                     never ride the chat; a replacement keeps the stored record's \
                     headers as they are"
                        .into(),
                );
            }
            Ok(CatalogChange::UpsertModelRecord {
                provider_id,
                record: Box::new(record),
            })
        }
        "upsert_custom_provider" => {
            let provider = value
                .get("provider")
                .cloned()
                .ok_or_else(|| "\"provider\" is required".to_string())?;
            let provider: CustomProvider = serde_json::from_value(provider).map_err(|error| {
                format!("\"provider\" is not a custom provider definition: {error}")
            })?;
            Ok(CatalogChange::UpsertCustomProvider { provider })
        }
        "remove_custom_provider" => Ok(CatalogChange::RemoveCustomProvider {
            provider_id: input_str(value, "providerId")?.to_string(),
        }),
        "remove_model_record" => Ok(CatalogChange::RemoveModelRecord {
            provider_id: input_str(value, "providerId")?.to_string(),
            model_id: input_str(value, "modelId")?.to_string(),
        }),
        "set_hidden_models" => {
            let provider_id = input_str(value, "providerId")?.to_string();
            let ids = value
                .get("modelIds")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| "\"modelIds\" must be an array of strings".to_string())?;
            let mut model_ids = BTreeSet::new();
            for id in ids {
                let id = id
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| "\"modelIds\" must contain non-empty strings".to_string())?;
                model_ids.insert(id.to_string());
            }
            Ok(CatalogChange::SetHiddenModels {
                provider_id,
                model_ids,
            })
        }
        other => Err(format!(
            "unknown action {other:?}; valid actions: upsert_model_record, \
             upsert_custom_provider, remove_custom_provider, remove_model_record, \
             set_hidden_models"
        )),
    }
}

// ---------------------------------------------------------------------------
// The stored proposal
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredProposal {
    pub(crate) id: String,
    pub(crate) summary: String,
    pub(crate) changes: Vec<CatalogChange>,
    /// [`baseline_fingerprint`] of the providers the batch touches, taken
    /// when the proposal was built. A hash, not the slice: stored record
    /// headers are secrets and the proposal is persisted (ADR-0037).
    pub(crate) baseline: String,
    /// Creation time (unix ms).
    pub(crate) created_at: i64,
}

/// Remembers a proposal on its chat and returns its id. A new proposal
/// replaces every stored one touching any of the same providers (their
/// cards read as superseded), and the oldest fall out past
/// [`PROPOSAL_CAP`]. Persisted with the chat's Provider Mode state.
pub(crate) fn store_proposal(
    chat: &ChatRuntime,
    changes: Vec<CatalogChange>,
    summary: String,
    baseline: &ProviderSettingsSnapshot,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let touched = touched_providers(&changes);
    let baseline = baseline_fingerprint(baseline, &touched);
    {
        let mut proposals = chat.proposals.lock().unwrap_or_else(|e| e.into_inner());
        proposals.retain(|stored| touched_providers(&stored.changes).is_disjoint(&touched));
        proposals.push_back(StoredProposal {
            id: id.clone(),
            summary,
            changes,
            baseline,
            created_at: chrono::Utc::now().timestamp_millis(),
        });
        while proposals.len() > PROPOSAL_CAP {
            proposals.pop_front();
        }
    }
    chat.save_provider_mode();
    id
}

/// The provider ids a batch reads or writes.
pub(crate) fn touched_providers(changes: &[CatalogChange]) -> BTreeSet<String> {
    changes
        .iter()
        .map(|change| change.provider_id().to_string())
        .collect()
}

/// The staleness gate's key (ADR-0037): a hash of every settings entry
/// under the touched providers. A write to any other provider leaves it —
/// and so the proposal — intact; any change under a touched one stales it.
/// Hashed over canonical (key-sorted) JSON so it is stable across restarts.
pub(crate) fn baseline_fingerprint(
    snapshot: &ProviderSettingsSnapshot,
    touched: &BTreeSet<String>,
) -> String {
    fn canonical(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (ix, key) in keys.into_iter().enumerate() {
                    if ix > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String(key.clone()).to_string());
                    out.push(':');
                    canonical(&map[key], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(items) => {
                out.push('[');
                for (ix, item) in items.iter().enumerate() {
                    if ix > 0 {
                        out.push(',');
                    }
                    canonical(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let slice: Vec<serde_json::Value> = touched
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "customModels": snapshot.custom_models.get(id),
                "modelRecords": snapshot.model_records.get(id),
                "customProvider": snapshot.custom_providers.get(id),
                "hiddenModels": snapshot.hidden_models.get(id),
            })
        })
        .collect();
    let mut text = String::new();
    canonical(&serde_json::Value::Array(slice), &mut text);
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub(crate) fn stored_proposal(chat: &ChatRuntime, id: &str) -> Option<StoredProposal> {
    chat.proposals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .find(|proposal| proposal.id == id)
        .cloned()
}

/// The gate's pending-approval note: the stored proposal's summary, so the
/// user approves against a sentence, not a raw id.
pub(crate) fn approval_note(chat: &ChatRuntime, arguments: &serde_json::Value) -> Option<String> {
    let id = arguments
        .get("proposalId")
        .and_then(serde_json::Value::as_str)?;
    stored_proposal(chat, id).map(|proposal| proposal.summary)
}

// ---------------------------------------------------------------------------
// Proposal building: validate + diff
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ProposalOutcome {
    /// Every change does something; `lines` is the human-readable diff.
    Changes { lines: Vec<String>, summary: String },
    /// Nothing would change; `lines` says why per change.
    NoChanges { lines: Vec<String> },
}

/// The provider ids a batch itself defines, so a record for a provider the
/// same proposal creates validates instead of tripping "unknown provider".
fn planned_providers(changes: &[CatalogChange]) -> HashSet<&str> {
    changes
        .iter()
        .filter_map(|change| match change {
            CatalogChange::UpsertCustomProvider { provider } => Some(provider.id.as_str()),
            _ => None,
        })
        .collect()
}

fn json_field_diff(current: &serde_json::Value, proposed: &serde_json::Value) -> Vec<String> {
    let (Some(current), Some(proposed)) = (current.as_object(), proposed.as_object()) else {
        return vec![format!("record {} -> {}", current, proposed)];
    };
    let keys: BTreeSet<&str> = current
        .keys()
        .chain(proposed.keys())
        .map(String::as_str)
        .collect();
    keys.into_iter()
        .filter_map(|key| {
            let before = current.get(key).unwrap_or(&serde_json::Value::Null);
            let after = proposed.get(key).unwrap_or(&serde_json::Value::Null);
            let changed = before != after;
            let before = redact_json(before);
            let after = redact_json(after);
            changed.then(|| format!("{key} {before} -> {after}"))
        })
        .collect()
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "headers"
        || key.contains("authorization")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("token")
        || key.contains("secret")
}

fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if sensitive_key(key) {
                            serde_json::Value::String("<redacted>".into())
                        } else {
                            redact_json(value)
                        },
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_json).collect())
        }
        _ => value.clone(),
    }
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty())
        .unwrap_or(url)
}

/// One upsert's diff line(s): new, replacement (with the changed fields),
/// or a no-op with the reason.
fn record_diff(
    providers: &ProviderAdapter,
    provider_id: &str,
    record: &CoreModel,
) -> (
    Vec<String>,
    bool, /* no-op */
    bool, /* key destination moves */
) {
    let qualified = format!("{provider_id}/{}", record.id);
    let Some(current) = providers.resolve_model(provider_id, &qualified).ok() else {
        return (
            vec![format!(
                "+ {qualified}: new complete record {}",
                serde_json::to_string(&redact_json(
                    &serde_json::to_value(record).unwrap_or_default()
                ))
                .unwrap_or_default()
            )],
            false,
            false,
        );
    };
    if &current == record {
        return (
            vec![format!("= {qualified}: already exactly as proposed")],
            true,
            false,
        );
    }
    let current_json = serde_json::to_value(&current).unwrap_or_default();
    let proposed_json = serde_json::to_value(record).unwrap_or_default();
    let mut notes = json_field_diff(&current_json, &proposed_json);
    let key_moves = current.base_url != record.base_url
        && host_of(&current.base_url) != host_of(&record.base_url);
    if key_moves {
        notes.push("API key destination changes".to_string());
    }
    (
        vec![format!("~ {qualified}: {}", notes.join("; "))],
        false,
        key_moves,
    )
}

/// A replacement record never carries headers (`parse_change` rejects
/// them, the dump omits them), so it inherits the stored record's — an
/// update proposal cannot silently drop the user's secret headers.
fn inherit_headers(record: &CoreModel, existing: Option<&CoreModel>) -> CoreModel {
    let mut effective = record.clone();
    if effective.headers.is_none() {
        effective.headers = existing.and_then(|existing| existing.headers.clone());
    }
    effective
}

/// Validates the batch against the live catalog and produces its diff.
/// Errors mean "do not propose this" — the model reads and fixes them.
pub(crate) fn build_proposal(
    providers: &ProviderAdapter,
    changes: &[CatalogChange],
) -> Result<ProposalOutcome, String> {
    if changes.is_empty() {
        return Err("no changes given".into());
    }
    let planned = planned_providers(changes);
    let mut lines = Vec::new();
    let mut providers_touched: BTreeSet<String> = BTreeSet::new();
    let mut no_ops = 0usize;
    let mut key_moves = false;
    for change in changes {
        let provider_id = change.provider_id();
        let known = providers.provider_known(provider_id) || planned.contains(provider_id);
        match change {
            CatalogChange::UpsertModelRecord {
                provider_id,
                record,
            } => {
                if !known {
                    return Err(format!(
                        "unknown provider {provider_id:?} — define it with \
                         upsert_custom_provider in the same proposal"
                    ));
                }
                if let Some(problem) = provider_store::model_record_problem(provider_id, record) {
                    return Err(format!(
                        "record {}/{} is not servable: {problem}",
                        provider_id, record.id
                    ));
                }
                let existing = providers
                    .settings
                    .model_records_for(provider_id)
                    .into_iter()
                    .find(|stored| stored.id == record.id);
                let effective = inherit_headers(record, existing.as_ref());
                let (mut diff, no_op, moves) = record_diff(providers, provider_id, &effective);
                key_moves |= moves;
                lines.append(&mut diff);
                no_ops += no_op as usize;
                providers_touched.insert(provider_id.clone());
            }
            CatalogChange::UpsertCustomProvider { provider } => {
                if providers.provider_known(provider_id) {
                    return Err(format!(
                        "provider id {provider_id:?} already exists in the built-in catalog"
                    ));
                }
                if crate::provider_settings::custom_provider_problem(provider).is_some() {
                    let problem = crate::provider_settings::custom_provider_problem(provider)
                        .unwrap_or_default();
                    return Err(format!(
                        "custom provider {:?} is not servable: {problem}",
                        provider.id
                    ));
                }
                let exists = providers.settings.custom_provider(&provider.id);
                match exists.as_ref() {
                    Some(current) if current == provider => {
                        lines.push(format!(
                            "= provider {}: already exactly as proposed",
                            provider.id
                        ));
                        no_ops += 1;
                    }
                    Some(_) => {
                        let current = exists.as_ref().expect("matched Some");
                        let current_json = serde_json::to_value(current).unwrap_or_default();
                        let proposed_json = serde_json::to_value(provider).unwrap_or_default();
                        lines.push(format!(
                            "~ provider {}: {}",
                            provider.id,
                            json_field_diff(&current_json, &proposed_json).join("; ")
                        ));
                    }
                    None => lines.push(format!(
                        "+ provider {}: new complete definition {}",
                        provider.id,
                        serde_json::to_string(&redact_json(
                            &serde_json::to_value(provider).unwrap_or_default(),
                        ))
                        .unwrap_or_default()
                    )),
                }
                providers_touched.insert(provider.id.clone());
            }
            CatalogChange::RemoveCustomProvider { provider_id } => {
                if providers.settings.custom_provider(provider_id).is_none() {
                    lines.push(format!("= provider {provider_id}: no definition to remove"));
                    no_ops += 1;
                } else {
                    lines.push(format!("- provider {provider_id}"));
                    providers_touched.insert(provider_id.clone());
                }
            }
            CatalogChange::RemoveModelRecord {
                provider_id,
                model_id,
            } => {
                let known_record = providers
                    .settings
                    .model_records_for(provider_id)
                    .iter()
                    .any(|record| record.id == *model_id);
                if !known_record {
                    lines.push(format!(
                        "= {provider_id}/{model_id}: no stored record to remove"
                    ));
                    no_ops += 1;
                } else {
                    lines.push(format!("- {provider_id}/{model_id} (record)"));
                    providers_touched.insert(provider_id.clone());
                }
            }
            CatalogChange::SetHiddenModels {
                provider_id,
                model_ids,
            } => {
                if !known {
                    return Err(format!("unknown provider {provider_id:?}"));
                }
                for id in model_ids {
                    let planned_record = changes.iter().any(|change| {
                        matches!(
                            change,
                            CatalogChange::UpsertModelRecord {
                                provider_id: planned_provider,
                                record,
                            } if planned_provider == provider_id && record.id == *id
                        )
                    });
                    if !providers.has_model(provider_id, id) && !planned_record {
                        return Err(format!(
                            "unknown model for {provider_id}: {id} — hidden ids must exist"
                        ));
                    }
                }
                let current: BTreeSet<String> = providers
                    .settings
                    .hidden_models_for(provider_id)
                    .into_iter()
                    .collect();
                if &current == model_ids {
                    lines.push(format!("= hide {provider_id}: already exactly as proposed"));
                    no_ops += 1;
                } else {
                    let added: Vec<&String> = model_ids.difference(&current).collect();
                    let removed: Vec<&String> = current.difference(model_ids).collect();
                    let mut note = String::new();
                    if !added.is_empty() {
                        note.push_str(&format!(
                            "hide {}",
                            added
                                .iter()
                                .map(|id| id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if !removed.is_empty() {
                        if !note.is_empty() {
                            note.push_str("; ");
                        }
                        note.push_str(&format!(
                            "unhide {}",
                            removed
                                .iter()
                                .map(|id| id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    lines.push(format!("! {provider_id}: {note}"));
                    providers_touched.insert(provider_id.clone());
                }
            }
        }
    }
    if no_ops == changes.len() {
        return Ok(ProposalOutcome::NoChanges { lines });
    }
    let mut summary = format!(
        "Apply {} catalog change{} for {}",
        changes.len() - no_ops,
        if changes.len() - no_ops == 1 { "" } else { "s" },
        providers_touched
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    if key_moves {
        summary.push_str(" — includes baseUrl host changes (API key destination)");
    }
    Ok(ProposalOutcome::Changes { lines, summary })
}

/// Executes a stored proposal exactly as stored, re-validating each change
/// against the state at apply time. Returns one line per applied change.
pub(crate) fn apply_changes(
    providers: &ProviderAdapter,
    baseline: &ProviderSettingsSnapshot,
    changes: &[CatalogChange],
) -> Result<Vec<String>, String> {
    // The staleness gate (ADR-0029): the catalog must still accept the
    // batch as a real change — anything else means state moved between the
    // proposal and the approval, and the model must propose again.
    match build_proposal(providers, changes)? {
        ProposalOutcome::Changes { .. } => {}
        ProposalOutcome::NoChanges { .. } => {
            return Err(
                "the catalog changed since this proposal (it is now a no-op); \
                 run model_proposal again"
                    .into(),
            );
        }
    }
    let mut next = baseline.clone();
    let mut ordered = Vec::with_capacity(changes.len());
    ordered.extend(
        changes
            .iter()
            .filter(|change| matches!(change, CatalogChange::UpsertCustomProvider { .. })),
    );
    ordered.extend(
        changes
            .iter()
            .filter(|change| matches!(change, CatalogChange::UpsertModelRecord { .. })),
    );
    ordered.extend(changes.iter().filter(|change| {
        matches!(
            change,
            CatalogChange::RemoveCustomProvider { .. } | CatalogChange::RemoveModelRecord { .. }
        )
    }));
    ordered.extend(
        changes
            .iter()
            .filter(|change| matches!(change, CatalogChange::SetHiddenModels { .. })),
    );
    for change in ordered {
        match change {
            CatalogChange::UpsertModelRecord {
                provider_id,
                record,
            } => {
                if !providers.catalog_has_provider(provider_id)
                    && !next.custom_providers.contains_key(provider_id)
                {
                    return Err(format!("unknown provider {provider_id}"));
                }
                if let Some(problem) = provider_store::model_record_problem(provider_id, record) {
                    return Err(format!("record {}/{}: {problem}", provider_id, record.id));
                }
                let existing = next
                    .model_records
                    .get(provider_id)
                    .and_then(|records| records.get(&record.id));
                let effective = inherit_headers(record, existing);
                next.model_records
                    .entry(provider_id.clone())
                    .or_default()
                    .insert(effective.id.clone(), effective);
            }
            CatalogChange::UpsertCustomProvider { provider } => {
                providers
                    .validate_custom_provider(provider)
                    .map_err(|problem| format!("provider {}: {problem}", provider.id))?;
                next.custom_providers
                    .insert(provider.id.clone(), provider.clone());
            }
            CatalogChange::RemoveCustomProvider { provider_id } => {
                next.custom_providers.remove(provider_id);
            }
            CatalogChange::RemoveModelRecord {
                provider_id,
                model_id,
            } => {
                if let Some(records) = next.model_records.get_mut(provider_id) {
                    records.remove(model_id);
                    if records.is_empty() {
                        next.model_records.remove(provider_id);
                    }
                }
                if let Some(hidden) = next.hidden_models.get_mut(provider_id) {
                    hidden.remove(model_id);
                    if hidden.is_empty() {
                        next.hidden_models.remove(provider_id);
                    }
                }
            }
            CatalogChange::SetHiddenModels {
                provider_id,
                model_ids,
            } => {
                if !providers.catalog_has_provider(provider_id)
                    && !next.custom_providers.contains_key(provider_id)
                {
                    return Err(format!("unknown provider {provider_id}"));
                }
                for model_id in model_ids {
                    let exists = providers.catalog_has_model(provider_id, model_id)
                        || next
                            .model_records
                            .get(provider_id)
                            .is_some_and(|records| records.contains_key(model_id))
                        || next
                            .custom_models
                            .get(provider_id)
                            .is_some_and(|models| models.contains(model_id));
                    if !exists {
                        return Err(format!("unknown model for {provider_id}: {model_id}"));
                    }
                }
                if model_ids.is_empty() {
                    next.hidden_models.remove(provider_id);
                } else {
                    next.hidden_models
                        .insert(provider_id.clone(), model_ids.clone());
                }
            }
        }
    }
    let applied = changes
        .iter()
        .map(|change| match change {
            CatalogChange::UpsertModelRecord {
                provider_id,
                record,
            } => {
                format!("{}/{}", provider_id, record.id)
            }
            CatalogChange::UpsertCustomProvider { provider } => format!("provider {}", provider.id),
            CatalogChange::RemoveCustomProvider { provider_id } => {
                format!("provider {provider_id}")
            }
            CatalogChange::RemoveModelRecord {
                provider_id,
                model_id,
            } => {
                format!("{provider_id}/{model_id}")
            }
            CatalogChange::SetHiddenModels { provider_id, .. } => {
                format!("hidden set for {provider_id}")
            }
        })
        .collect();
    providers
        .settings
        .replace_if_unchanged(baseline, next)
        .map_err(|error| error.to_string())?;
    Ok(applied)
}

// ---------------------------------------------------------------------------
// The /models probe (read-only, best-effort)
// ---------------------------------------------------------------------------

/// `GET {baseUrl}/models` — the freshest model list a provider offers. The
/// key, when present, rides the Authorization header and never the output.
async fn probe_models(
    base_url: &str,
    api_dialect: &str,
    key: Option<&str>,
    cancellation: Option<CancellationToken>,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client.get(&url);
    if let Some(key) = key {
        if api_dialect.starts_with("anthropic") {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        } else {
            request = request.bearer_auth(key);
        }
    }
    let response = if let Some(cancellation) = cancellation.as_ref() {
        tokio::select! {
            _ = cancellation.cancelled() => return Err("cancelled".into()),
            response = request.send() => response.map_err(|error| error.to_string())?,
        }
    } else {
        request.send().await.map_err(|error| error.to_string())?
    };
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    const PROBE_BODY_CAP: usize = 1_048_576;
    if response
        .content_length()
        .is_some_and(|length| length > PROBE_BODY_CAP as u64)
    {
        return Err("response body is too large".into());
    }
    let body_bytes = if let Some(cancellation) = cancellation.as_ref() {
        tokio::select! {
            _ = cancellation.cancelled() => return Err("cancelled".into()),
            bytes = response.bytes() => bytes.map_err(|error| error.to_string())?,
        }
    } else {
        response.bytes().await.map_err(|error| error.to_string())?
    };
    if body_bytes.len() > PROBE_BODY_CAP {
        return Err("response body is too large".into());
    }
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|error| format!("body is not JSON: {error}"))?;
    Ok(parse_model_listing(&body))
}

/// Tolerant listing parse: OpenAI's `{"data":[{"id":…}]}` shape first, then
/// `{"models":[…]}`, a bare array of ids or objects, and string values.
fn parse_model_listing(body: &serde_json::Value) -> Vec<String> {
    let entries = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(|value| value.as_array())
        .cloned()
        .or_else(|| body.as_array().cloned())
        .unwrap_or_default();
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(id) => Some(id.clone()),
            serde_json::Value::Object(object) => object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect();
    ids.retain(|id| !id.trim().is_empty());
    ids.dedup();
    ids
}

/// A provider's probe target: its transport base URL and dialect, from the
/// definition (custom) or the first resolvable model (built-in).
fn probe_target(providers: &ProviderAdapter, provider_id: &str) -> Option<(String, String)> {
    if let Some(provider) = providers.settings.custom_provider(provider_id) {
        return Some((provider.base_url, provider.default_api));
    }
    let first = providers.models_for(provider_id).first()?.clone();
    let model = providers.resolve_model(provider_id, &first.id).ok()?;
    Some((model.base_url, model.api))
}

/// A tool call's `provider` draft: a definition the model resolved from
/// the docs but nobody has proposed or written yet — a key and probe
/// target before any proposal exists. Structurally validated here; the
/// public-host gate runs where the draft is used.
fn parse_draft(params: &serde_json::Value) -> Result<Option<CustomProvider>, String> {
    let Some(value) = params.get("provider").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let provider: CustomProvider = serde_json::from_value(value.clone())
        .map_err(|error| format!("\"provider\" is not a provider draft: {error}"))?;
    if let Some(problem) = crate::provider_settings::custom_provider_problem(&provider) {
        return Err(format!("\"provider\" draft is invalid: {problem}"));
    }
    Ok(Some(provider))
}

/// An inquiry probe's target and key rule: a draft probes its own planned
/// transport (through the public-host gate), carrying the key only for a
/// (provider, baseUrl) the user approved on this chat; without a draft the
/// stored target probes with the stored key.
fn inquiry_probe_plan(
    draft: Option<&CustomProvider>,
    approved: &HashSet<(String, String)>,
) -> (Option<(String, String)>, bool) {
    match draft {
        Some(draft) => (
            Some((draft.base_url.clone(), draft.default_api.clone())),
            planned_probe_key_allowed(approved, &draft.id, &draft.base_url),
        ),
        None => (None, true),
    }
}

/// Is this address reachable from the public internet only — i.e. not
/// loopback, private, link-local (the cloud metadata endpoints live
/// there), CGNAT, multicast, or otherwise non-routable space?
fn ip_is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && (octets[1] & 0xC0) == 64))
        }
        std::net::IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => ip_is_public(std::net::IpAddr::V4(v4)),
            None => {
                !(ip.is_loopback()
                    || ip.is_unicast_link_local()
                    || ip.is_unique_local()
                    || ip.is_multicast()
                    || ip.is_unspecified())
            }
        },
    }
}

/// The SSRF gate for a probe target the setup model chose but the user has
/// not applied: a planned baseUrl may only reach public hosts. Literal
/// addresses are classified directly; hostnames must resolve, and every
/// resolved address must be public (a private answer refuses the probe).
/// The gap between this check and the request's own resolution stays open
/// to DNS rebinding — accepted for a GET whose readback is a filtered model
/// listing, not raw bodies.
async fn planned_probe_problem(base_url: &str) -> Option<String> {
    let url = match reqwest::Url::parse(base_url) {
        Ok(url) => url,
        Err(error) => return Some(format!("the baseUrl does not parse: {error}")),
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Some("the baseUrl is not http(s)".into());
    }
    let Some(host) = url
        .host_str()
        .map(|host| host.trim_matches(|c| c == '[' || c == ']'))
        .map(str::to_string)
    else {
        return Some("the baseUrl has no host".into());
    };
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return (!ip_is_public(ip)).then(|| format!("{host} is not a public address"));
    }
    let port = url.port_or_known_default().unwrap_or(0);
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => {
            let resolved: Vec<std::net::SocketAddr> = addrs.collect();
            if resolved.is_empty() {
                Some(format!("{host} does not resolve"))
            } else if resolved.iter().any(|addr| !ip_is_public(addr.ip())) {
                Some(format!("{host} resolves to a non-public address"))
            } else {
                None
            }
        }
        Err(_) => Some(format!("{host} does not resolve")),
    }
}

fn probe_section(
    providers: Arc<ProviderAdapter>,
    provider_id: &str,
    override_target: Option<(String, String)>,
    allow_key: bool,
    cancellation: Option<CancellationToken>,
) -> BoxFuture<'static, String> {
    let provider_id = provider_id.to_string();
    Box::pin(async move {
        let (base_url, dialect) = match override_target {
            // A planned target is setup-model input nobody has approved
            // yet: it must pass the SSRF filter before any request.
            Some((base_url, dialect)) => {
                if let Some(problem) = planned_probe_problem(&base_url).await {
                    return format!(
                        "probe {provider_id}: refused ({problem}) — a planned baseUrl \
                         becomes probeable once the proposal is applied; a settled key \
                         never bypasses this gate"
                    );
                }
                (base_url, dialect)
            }
            // A stored target is the user's own endpoint choice — probing it
            // goes exactly where a normal request would, loopback included.
            None => {
                let Some((base_url, dialect)) = probe_target(&providers, &provider_id) else {
                    return format!("probe {provider_id}: no transport to probe");
                };
                (base_url, dialect)
            }
        };
        let key = if allow_key {
            providers.credentials.reveal_key(&provider_id).await
        } else {
            None
        };
        let key_note = if key.is_some() { ", key attached" } else { "" };
        match probe_models(&base_url, &dialect, key.as_deref(), cancellation).await {
            Ok(ids) if ids.is_empty() => {
                format!("probe {provider_id}: endpoint returned an empty listing")
            }
            Ok(ids) => {
                let total = ids.len();
                let shown: Vec<&str> = ids
                    .iter()
                    .take(PROBE_LISTING_CAP)
                    .map(String::as_str)
                    .collect();
                let suffix = if total > PROBE_LISTING_CAP {
                    format!(" … ({} total)", total)
                } else {
                    String::new()
                };
                format!(
                    "probe {provider_id} ({}{}): {}{}",
                    host_of(&base_url),
                    key_note,
                    shown.join(", "),
                    suffix
                )
            }
            Err(problem) => format!(
                "probe {provider_id} ({}{}): unavailable ({problem}) — {}continuing \
                 without it",
                host_of(&base_url),
                key_note,
                if key.is_some() {
                    "the key was attached and still rejected — it is wrong; "
                } else {
                    "the provider may require a key or not expose /models; "
                }
            ),
        }
    })
}

/// Drops one stored proposal (the card's Discard). Returns
/// `false` when the chat holds no proposal under that id.
pub(crate) fn discard_stored(chat: &ChatRuntime, proposal_id: &str) -> bool {
    let mut proposals = chat
        .proposals
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let before = proposals.len();
    proposals.retain(|proposal| proposal.id != proposal_id);
    let discarded = proposals.len() != before;
    drop(proposals);
    if discarded {
        chat.save_provider_mode();
    }
    discarded
}

/// The proposal card's Write button (ADR-0037): executes a stored
/// proposal under the same revalidation the tool path used. The human
/// approval is the button itself — no agent is involved. A successful
/// apply CONSUMES the proposal (same path as discard): an already-written
/// change left listed would invite a second Write, which the staleness
/// gate can only reject as a no-op.
pub(crate) fn apply_stored(
    providers: &ProviderAdapter,
    chat: &ChatRuntime,
    proposal_id: &str,
) -> Result<Vec<String>, String> {
    let proposal = stored_proposal(chat, proposal_id).ok_or_else(|| {
        format!(
            "no stored proposal {proposal_id:?} on this chat — proposals do not survive \
             a restart; ask the assistant to propose again"
        )
    })?;
    // Only the touched providers must be as proposed; the whole-snapshot
    // CAS inside `apply_changes` then guards just this read→write window.
    let live = providers.settings.snapshot();
    if baseline_fingerprint(&live, &touched_providers(&proposal.changes)) != proposal.baseline {
        return Err(STALE_PROPOSAL.into());
    }
    let applied = apply_changes(providers, &live, &proposal.changes)?;
    discard_stored(chat, proposal_id);
    Ok(applied)
}

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

fn text_result(text: String, details: serde_json::Value) -> Result<AgentToolResult, String> {
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text,
            ..Default::default()
        })],
        details,
        ..Default::default()
    })
}

fn proposal_parameters_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "changes": {
                "type": "array",
                "description": "The exact catalog changes to prepare; omit to only inspect a provider",
                "items": { "type": "object" }
            },
            "providerId": {
                "type": "string",
                "description": "Provider to inspect or probe; omit it entirely to list the \
    organizations and their providers"
            },
            "modelId": {
                "type": "string",
                "description": "When inspecting: dump this one model's complete record JSON"
            },
            "probe": {
                "type": "boolean",
                "description": "Also GET {baseUrl}/models live against the provider (read-only)"
            },
            "provider": provider_draft_schema()
        }
    })
}

fn provider_draft_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "description": "A provider not yet in the catalog or any proposal: \
    {id, name, baseUrl, defaultApi} as resolved from its docs",
        "properties": {
            "id": { "type": "string" },
            "name": { "type": "string" },
            "baseUrl": { "type": "string" },
            "defaultApi": { "type": "string" }
        },
        "required": ["id", "name", "baseUrl", "defaultApi"]
    })
}

async fn run_proposal_tool(
    providers: Arc<ProviderAdapter>,
    chat: Arc<ChatRuntime>,
    params: &serde_json::Value,
    cancellation: Option<CancellationToken>,
) -> Result<AgentToolResult, String> {
    let probe = params.get("probe").and_then(serde_json::Value::as_bool) == Some(true);
    let provider_id = params
        .get("providerId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let raw_changes = params.get("changes").and_then(|value| value.as_array());
    let Some(raw_changes) = raw_changes else {
        let draft = parse_draft(params)?;
        let provider_id = match (&draft, provider_id) {
            (Some(draft), Some(id)) if id != draft.id => {
                return Err(format!(
                    "providerId {id:?} does not match the provider draft's id {:?}",
                    draft.id
                ));
            }
            (Some(draft), _) => Some(draft.id.clone()),
            (None, id) => id,
        };
        let (probe_target, probe_key) =
            inquiry_probe_plan(draft.as_ref(), &approved_key_destinations(&chat));
        // Inquiry mode: one model's complete record (the replacement
        // template), the provider listing (the no-op detection the GLM
        // rehearsal made the first step), or — with no providerId — the
        // organization-level listing the disambiguation step runs on.
        let Some(provider_id) = provider_id else {
            let lines = org_listing(&providers).await;
            return text_result(lines.join("\n"), json!({ "inquiry": "providers" }));
        };
        if let Some(model_id) = params
            .get("modelId")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let mut lines = record_dump(&providers, &provider_id, model_id)?;
            if probe {
                lines.push(String::new());
                lines.push(
                    probe_section(
                        providers.clone(),
                        &provider_id,
                        probe_target.clone(),
                        probe_key,
                        cancellation.clone(),
                    )
                    .await,
                );
            }
            return text_result(
                lines.join("\n"),
                json!({ "inquiry": provider_id, "modelId": model_id }),
            );
        }
        let mut lines = local_listing(&providers, &provider_id);
        if probe {
            lines.push(String::new());
            lines.push(
                probe_section(
                    providers.clone(),
                    &provider_id,
                    probe_target,
                    probe_key,
                    cancellation.clone(),
                )
                .await,
            );
        }
        return text_result(lines.join("\n"), json!({ "inquiry": provider_id }));
    };
    let mut changes = Vec::new();
    for value in raw_changes {
        changes.push(
            parse_change(value)
                .map_err(|problem| format!("changes[{}]: {problem}", changes.len()))?,
        );
    }
    let baseline = providers.settings.snapshot();
    let outcome = build_proposal(&providers, &changes)?;
    let mut lines = match &outcome {
        ProposalOutcome::Changes { lines, .. } => lines.clone(),
        ProposalOutcome::NoChanges { lines } => lines.clone(),
    };
    match outcome {
        ProposalOutcome::Changes {
            summary,
            lines: diff,
        } => {
            let probe_providers: BTreeSet<String> = changes
                .iter()
                .map(|change| change.provider_id().to_string())
                .collect();
            if probe {
                // A provider this batch defines has no stored transport
                // yet — probe the planned definition, not the (absent)
                // stored one.
                let planned: std::collections::HashMap<String, (String, String)> = changes
                    .iter()
                    .filter_map(|change| match change {
                        CatalogChange::UpsertCustomProvider { provider } => Some((
                            provider.id.clone(),
                            (provider.base_url.clone(), provider.default_api.clone()),
                        )),
                        _ => None,
                    })
                    .collect();
                let approved = approved_key_destinations(&chat);
                lines.push(String::new());
                for provider_id in probe_providers {
                    let target = planned.get(&provider_id).cloned();
                    // A provider this batch defines probes its planned
                    // transport; the key rides only when the user approved
                    // exactly that (provider, baseUrl) via a Key request.
                    // A provider the batch only touches keeps today's
                    // keyless changes-mode probe.
                    let allow_key = match &target {
                        Some((base_url, _)) => {
                            planned_probe_key_allowed(&approved, &provider_id, base_url)
                        }
                        None => false,
                    };
                    lines.push(
                        probe_section(
                            providers.clone(),
                            &provider_id,
                            target,
                            allow_key,
                            cancellation.clone(),
                        )
                        .await,
                    );
                }
            }
            let id = store_proposal(&chat, changes, summary.clone(), &baseline);
            lines.push(String::new());
            lines.push(format!("proposalId: {id}"));
            lines.push(
                "Proposal stored. A proposal card with this diff appears in the conversation; \
                 give a short summary and stop — the user writes it from the card. Nothing \
                 is written until they do."
                    .into(),
            );
            text_result(
                lines.join("\n"),
                json!({ "proposalId": id, "summary": summary, "lines": diff }),
            )
        }
        ProposalOutcome::NoChanges { .. } => text_result(
            format!("No changes needed:\n{}", lines.join("\n")),
            json!({ "no_op": true }),
        ),
    }
}

/// One model's complete record as JSON — the template a replacement
/// proposal copies, so `compat`/`thinkingLevelMap` never have to be guessed
/// (or excavated from vendored sources). Header values are secret: the dump
/// omits the field and a replacement proposal keeps the stored headers.
fn record_dump(
    providers: &ProviderAdapter,
    provider_id: &str,
    model_id: &str,
) -> Result<Vec<String>, String> {
    let bare = model_id
        .strip_prefix(&format!("{provider_id}/"))
        .unwrap_or(model_id);
    let qualified = format!("{provider_id}/{bare}");
    let model = providers
        .resolve_model(provider_id, &qualified)
        .map_err(|_| format!("unknown model: {qualified}"))?;
    let hidden = providers
        .settings
        .hidden_models_for(provider_id)
        .iter()
        .any(|id| id == bare);
    let mut lines = vec![format!(
        "{qualified} (hidden: {hidden}) — the complete record as the catalog serves it:"
    )];
    let mut json = serde_json::to_value(&model)
        .map_err(|error| format!("record does not serialize: {error}"))?;
    // The field is dropped whole, not redacted in place, so a copied
    // template still deserializes into a valid proposal record.
    if json
        .get("headers")
        .is_some_and(|headers| !headers.is_null())
    {
        json.as_object_mut().map(|object| object.remove("headers"));
        lines.push(
            "(this record carries custom headers — omitted here because header \
             values are secret; a replacement keeps them as stored, and they are \
             managed in Settings)"
                .into(),
        );
    }
    lines.push(
        serde_json::to_string_pretty(&json)
            .map_err(|error| format!("record does not serialize: {error}"))?,
    );
    Ok(lines)
}

/// The provider's live catalog view, as the model sees it.
/// The organization-grouped provider listing behind `model_proposal`'s
/// providerless inquiry mode — the disambiguation step's data. A request
/// like "update Xiaomi" names an organization; the variants under it are
/// the concrete, RPC-addressable providers, and only the user can pick
/// between them.
async fn org_listing(providers: &ProviderAdapter) -> Vec<String> {
    let mut lines = vec![
        "Providers by organization (one organization often carries several \
         providers; resolve the request to ONE provider id):"
            .to_string(),
    ];
    for row in providers.providers().await {
        if row.variants.len() == 1 {
            let variant = &row.variants[0];
            lines.push(format!(
                "{} — {}{}",
                variant.id.0,
                variant.name,
                configured_note(variant.configured)
            ));
        } else {
            lines.push(format!("{} — {}:", row.id, row.name));
            for variant in &row.variants {
                lines.push(format!(
                    "  {} — {}{}",
                    variant.id.0,
                    variant.name,
                    configured_note(variant.configured)
                ));
            }
        }
    }
    lines
}

fn configured_note(configured: bool) -> &'static str {
    if configured { " [configured]" } else { "" }
}

fn local_listing(providers: &ProviderAdapter, provider_id: &str) -> Vec<String> {
    let hidden: HashSet<String> = providers
        .settings
        .hidden_models_for(provider_id)
        .into_iter()
        .collect();
    let mut lines = vec![format!("{provider_id}:")];
    let rows = providers.models_for(provider_id);
    if rows.is_empty() {
        lines.push("  (no models)".into());
        return lines;
    }
    for row in rows {
        let mut flags = Vec::new();
        if row.custom {
            flags.push("custom");
        }
        if hidden.contains(
            row.id
                .strip_prefix(&format!("{provider_id}/"))
                .unwrap_or(&row.id),
        ) {
            flags.push("hidden");
        }
        let window = row
            .context_window
            .map(|window| window.to_string())
            .unwrap_or_else(|| "unknown".into());
        lines.push(format!(
            "  {} — ctx {}{}",
            row.id,
            window,
            if flags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", flags.join(", "))
            }
        ));
    }
    lines
}

pub(crate) fn create_model_proposal_tool(
    providers: Arc<ProviderAdapter>,
    chat: Arc<ChatRuntime>,
) -> AgentTool {
    AgentTool {
        name: "model_proposal".into(),
        label: "Model Proposal".into(),
        description: PROPOSAL_DESCRIPTION.into(),
        parameters: proposal_parameters_schema(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                if signal.is_some_and(CancellationToken::is_cancelled) {
                    return Box::pin(futures::future::ready(Err(
                        "model_proposal cancelled".to_string()
                    )))
                        as BoxFuture<'static, Result<AgentToolResult, String>>;
                }
                let providers = providers.clone();
                let chat = chat.clone();
                let params = params.clone();
                let cancellation = signal.cloned();
                Box::pin(
                    async move { run_proposal_tool(providers, chat, &params, cancellation).await },
                ) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

// ---------------------------------------------------------------------------
// The Key request (ADR-0031)
// ---------------------------------------------------------------------------

/// The chat's pending Key request: what the key card stands for and the
/// settle RPC consumes. Deliberately key-free — only where the
/// key would go, never a key value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingKeyRequest {
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    /// The base URL the key would be sent to when probing — the string
    /// the user approves by saving.
    pub(crate) destination: String,
}

/// The card's destination: a stored custom provider's own baseUrl, else
/// the newest unapplied proposal's planned definition, else the catalog's
/// transport for a known provider, else the call's draft — which must
/// pass the planned probe's public-host gate. An error means the id names
/// nothing the engine can address, or the draft was refused.
async fn key_request_target(
    providers: &ProviderAdapter,
    chat: &ChatRuntime,
    provider_id: &str,
    draft: Option<CustomProvider>,
) -> Result<PendingKeyRequest, String> {
    if let Some(request) = known_key_target(providers, chat, provider_id).await {
        return Ok(request);
    }
    let Some(draft) = draft else {
        return Err(format!(
            "unknown provider {provider_id:?} — resolve it to a catalog id, or pass a \
             `provider` draft {{id, name, baseUrl, defaultApi}} from its docs"
        ));
    };
    if draft.id != provider_id {
        return Err(format!(
            "providerId {provider_id:?} does not match the provider draft's id {:?}",
            draft.id
        ));
    }
    if let Some(problem) = planned_probe_problem(&draft.base_url).await {
        return Err(format!(
            "the provider draft's baseUrl is refused ({problem}) — a key is only \
             requested for a public endpoint"
        ));
    }
    Ok(PendingKeyRequest {
        provider_id: draft.id,
        provider_name: draft.name,
        destination: draft.base_url,
    })
}

async fn known_key_target(
    providers: &ProviderAdapter,
    chat: &ChatRuntime,
    provider_id: &str,
) -> Option<PendingKeyRequest> {
    if let Some(provider) = providers.settings.custom_provider(provider_id) {
        return Some(PendingKeyRequest {
            provider_id: provider_id.to_string(),
            provider_name: provider.name,
            destination: provider.base_url,
        });
    }
    let planned = {
        let proposals = chat.proposals.lock().unwrap_or_else(|e| e.into_inner());
        proposals.iter().rev().find_map(|proposal| {
            proposal.changes.iter().find_map(|change| match change {
                CatalogChange::UpsertCustomProvider { provider } if provider.id == provider_id => {
                    Some(provider.clone())
                }
                _ => None,
            })
        })
    };
    if let Some(provider) = planned {
        return Some(PendingKeyRequest {
            provider_id: provider_id.to_string(),
            provider_name: provider.name,
            destination: provider.base_url,
        });
    }
    if !providers.provider_known(provider_id) {
        return None;
    }
    let (base_url, _) = probe_target(providers, provider_id)?;
    let provider_name = providers
        .providers()
        .await
        .iter()
        .flat_map(|row| row.variants.iter())
        .find(|variant| variant.id.0 == provider_id)
        .map(|variant| variant.name.clone())
        .unwrap_or_else(|| provider_id.to_string());
    Some(PendingKeyRequest {
        provider_id: provider_id.to_string(),
        provider_name,
        destination: base_url,
    })
}

/// May a planned-target probe carry the stored key (ADR-0031)? The user
/// approved exactly the (provider, baseUrl) the card showed, on this chat,
/// in this session — a changed baseUrl re-arms, and nothing else ever
/// matches. Pure so the triple rules are unit-testable.
pub(crate) fn planned_probe_key_allowed(
    approved: &HashSet<(String, String)>,
    provider_id: &str,
    base_url: &str,
) -> bool {
    approved.contains(&(provider_id.to_string(), base_url.to_string()))
}

/// The chat's session approvals, snapshotted for one probe pass.
pub(crate) fn approved_key_destinations(chat: &ChatRuntime) -> HashSet<(String, String)> {
    chat.approved_key_destinations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The settle's save notice — the fixed message that continues the setup
/// chat once a key is stored. Engine-owned so the wording and the setup
/// prompt stay one contract.
pub(crate) fn key_saved_notice(provider_id: &str, destination: &str) -> String {
    format!(
        "API key saved for {provider_id} (probes send it to {destination}). \
         Re-probe the provider's model list and continue."
    )
}

/// The settle's dismissal notice — the counterpart that keeps the flow
/// alive when the user declines.
pub(crate) fn key_dismissed_notice(provider_id: &str) -> String {
    format!(
        "The user dismissed the key request for {provider_id}. \
         Continue with web research."
    )
}

const KEY_REQUEST_DESCRIPTION: &str = "Ask the user for a provider's API key through a \
key card in the conversation. Call this as soon as the provider's docs say requests need a \
key, or when a `model_proposal` probe fails with HTTP 401/403 — NEVER ask the user to paste \
a key in chat. Parameters: `providerId`, and `provider` (the draft {id, name, baseUrl, \
defaultApi} from its docs) when the provider is in neither the catalog nor a proposal yet — \
the draft's baseUrl must be a public https endpoint. The card collects the key locally and \
saves it to the credential store; the key is never sent to you. After calling, tell the user \
to enter the key in the card and STOP your turn. Your next user message reports the \
outcome: 'API key saved for …' means re-run the probe with `probe: true` (inquiry mode — \
`providerId`, plus the same `provider` draft if you used one; the same `changes` again for \
a provider your proposal defines); 'dismissed' means continue with web research. A probe \
marked 'key attached' that still fails means the key itself is wrong — say so instead of \
requesting again.";

pub(crate) fn create_request_provider_key_tool(
    providers: Arc<ProviderAdapter>,
    chat: Arc<ChatRuntime>,
) -> AgentTool {
    AgentTool {
        name: "request_provider_key".into(),
        label: "Request API Key".into(),
        description: KEY_REQUEST_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "providerId": {
                    "type": "string",
                    "description": "The provider the key unlocks — a catalog id, \
        one a stored proposal defines, or the draft's id"
                },
                "provider": provider_draft_schema()
            },
            "required": ["providerId"]
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let providers = providers.clone();
                let chat = chat.clone();
                let params = params.clone();
                Box::pin(async move {
                    let provider_id = params
                        .get("providerId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .ok_or_else(|| "\"providerId\" is required".to_string())?
                        .to_string();
                    let draft = parse_draft(&params)?;
                    let request =
                        key_request_target(&providers, &chat, &provider_id, draft).await?;
                    let destination = request.destination.clone();
                    let provider_name = request.provider_name.clone();
                    *chat.key_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(request);
                    chat.save_provider_mode();
                    text_result(
                        format!(
                            "Key request shown for {provider_id} — a key card appears in the \
                             conversation and sends the key only to {destination}. Tell the \
                             user to enter it there, then STOP this turn; your next user \
                             message reports the outcome.",
                        ),
                        json!({
                            "providerId": provider_id,
                            "providerName": provider_name,
                            "destination": destination,
                        }),
                    )
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(data_dir: &std::path::Path) -> ProviderAdapter {
        ProviderAdapter::new(
            Arc::new(crate::credentials::HoltCredentialStore::load(data_dir).unwrap()),
            Arc::new(crate::provider_settings::ProviderSettingsStore::load(data_dir).unwrap()),
            crate::provider_store::load(data_dir),
        )
    }

    fn record(provider: &str, id: &str, base_url: &str) -> CoreModel {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": base_url,
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 1.0, "output": 2.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
            "contextWindow": 200_000,
            "maxTokens": 8_192,
        }))
        .unwrap()
    }

    fn change(provider: &str, id: &str, base_url: &str) -> CatalogChange {
        CatalogChange::UpsertModelRecord {
            provider_id: provider.to_string(),
            record: Box::new(record(provider, id, base_url)),
        }
    }

    #[tokio::test]
    async fn the_providerless_inquiry_lists_organizations_and_variants() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let listing = org_listing(&providers).await;
        // Every addressable provider appears; an organization carrying
        // several providers indents its variants under the org row — the
        // shape the disambiguation step asks the user to pick from.
        let multi = providers
            .providers()
            .await
            .into_iter()
            .find(|row| row.variants.len() > 1)
            .expect("the catalog carries a multi-provider organization");
        let org_line = listing
            .iter()
            .position(|line| line.starts_with(&format!("{} — ", multi.id.0)))
            .expect("org row present");
        for variant in &multi.variants {
            assert!(
                listing[org_line..]
                    .iter()
                    .any(|line| line.starts_with("  ") && line.contains(variant.id.0.as_str())),
                "variant {} not indented under {}",
                variant.id.0,
                multi.id.0
            );
        }
    }

    #[test]
    fn changes_parse_from_tool_arguments() {
        let parsed = parse_change(&serde_json::json!({
            "action": "upsert_model_record",
            "providerId": "openai",
            "record": record("openai", "gpt-x", "https://api.openai.com/v1"),
        }))
        .unwrap();
        assert!(matches!(parsed, CatalogChange::UpsertModelRecord { .. }));

        let hidden = parse_change(&serde_json::json!({
            "action": "set_hidden_models",
            "providerId": "kimi",
            "modelIds": ["k2", "k2.5"],
        }))
        .unwrap();
        assert!(matches!(hidden, CatalogChange::SetHiddenModels { .. }));

        assert!(parse_change(&serde_json::json!({ "action": "explode" })).is_err());
        assert!(parse_change(&serde_json::json!({ "action": "upsert_model_record" })).is_err());
    }

    #[test]
    fn a_proposed_record_cannot_carry_headers() {
        let mut with_headers = record("openai", "gpt-x", "https://api.openai.com/v1");
        with_headers.headers = Some(std::collections::BTreeMap::from([(
            "X-Api-Key".to_string(),
            "secret-value".to_string(),
        )]));
        let problem = parse_change(&serde_json::json!({
            "action": "upsert_model_record",
            "providerId": "openai",
            "record": with_headers,
        }))
        .unwrap_err();
        assert!(problem.contains("headers"));
    }

    #[test]
    fn a_new_record_proposes_and_a_repeat_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let changes = vec![change(
            "openai",
            "gpt-via-proposal",
            "https://api.openai.com/v1",
        )];
        let ProposalOutcome::Changes { lines, summary } =
            build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines[0].contains("+ openai/gpt-via-proposal"));
        assert!(summary.contains("openai"));

        // Applying it for real, then re-proposing the same thing: no-op.
        apply_changes(&providers, &providers.settings.snapshot(), &changes).unwrap();
        let ProposalOutcome::NoChanges { lines } = build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a no-op");
        };
        assert!(lines[0].contains("already exactly as proposed"));
        // And apply rejects the stale batch.
        assert!(apply_changes(&providers, &providers.settings.snapshot(), &changes).is_err());
    }

    #[test]
    fn an_approved_proposal_cannot_overwrite_a_newer_value() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let proposed = change("openai", "gpt-cas", "https://api.openai.com/v1");
        let baseline = providers.settings.snapshot();
        assert!(matches!(
            build_proposal(&providers, std::slice::from_ref(&proposed)).unwrap(),
            ProposalOutcome::Changes { .. }
        ));

        let mut newer = record("openai", "gpt-cas", "https://api.openai.com/v1");
        newer.context_window = 300_000;
        providers
            .settings
            .upsert_model_record("openai", newer.clone())
            .unwrap();

        let error = apply_changes(&providers, &baseline, &[proposed]).unwrap_err();
        assert!(error.contains("changed since this proposal"));
        assert_eq!(
            providers
                .resolve_model("openai", "openai/gpt-cas")
                .unwrap()
                .context_window,
            newer.context_window
        );
    }

    fn propose(providers: &ProviderAdapter, chat: &ChatRuntime, change: CatalogChange) -> String {
        let changes = vec![change];
        let ProposalOutcome::Changes { summary, .. } = build_proposal(providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        store_proposal(chat, changes, summary, &providers.settings.snapshot())
    }

    #[test]
    fn a_write_stales_only_proposals_on_the_same_provider() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let chat = ChatRuntime::new();
        let openai = propose(
            &providers,
            &chat,
            change("openai", "gpt-one", "https://api.openai.com/v1"),
        );
        // A second chat: on one chat the newer proposal would replace it.
        let other_chat = ChatRuntime::new();
        let openai_again = propose(
            &providers,
            &other_chat,
            change("openai", "gpt-two", "https://api.openai.com/v1"),
        );
        let anthropic = propose(
            &providers,
            &chat,
            change("anthropic", "claude-x", "https://api.anthropic.com"),
        );

        apply_stored(&providers, &chat, &openai).unwrap();
        let error = apply_stored(&providers, &other_chat, &openai_again).unwrap_err();
        assert!(error.contains("changed since this proposal"), "{error}");
        apply_stored(&providers, &chat, &anthropic).unwrap();
    }

    #[test]
    fn a_new_proposal_replaces_overlapping_ones_on_its_chat() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let chat = ChatRuntime::new();
        let first = propose(
            &providers,
            &chat,
            change("openai", "gpt-one", "https://api.openai.com/v1"),
        );
        let anthropic = propose(
            &providers,
            &chat,
            change("anthropic", "claude-x", "https://api.anthropic.com"),
        );
        let second = propose(
            &providers,
            &chat,
            change("openai", "gpt-two", "https://api.openai.com/v1"),
        );
        assert!(stored_proposal(&chat, &first).is_none());
        assert!(stored_proposal(&chat, &anthropic).is_some());
        assert!(stored_proposal(&chat, &second).is_some());
    }

    #[test]
    fn an_untouched_provider_change_does_not_stale_a_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let chat = ChatRuntime::new();
        let id = propose(
            &providers,
            &chat,
            change("openai", "gpt-one", "https://api.openai.com/v1"),
        );
        providers
            .settings
            .upsert_model_record(
                "anthropic",
                record("anthropic", "claude-x", "https://api.anthropic.com"),
            )
            .unwrap();
        apply_stored(&providers, &chat, &id).unwrap();
    }

    #[test]
    fn stored_proposals_round_trip_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let chat = ChatRuntime::new();
        let id = propose(
            &providers,
            &chat,
            change("openai", "gpt-one", "https://api.openai.com/v1"),
        );
        let stored = stored_proposal(&chat, &id).unwrap();
        let json = serde_json::to_value(&stored).unwrap();
        assert_eq!(json["changes"][0]["action"], "upsert_model_record");
        assert_eq!(json["changes"][0]["providerId"], "openai");
        let back: StoredProposal = serde_json::from_value(json).unwrap();
        assert_eq!(back, stored);
    }

    #[test]
    fn a_base_url_host_change_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let first = providers.models_for("openai")[0].clone();
        let bare = first.id.strip_prefix("openai/").unwrap().to_string();
        let replacement = {
            let mut record = record("openai", &bare, "https://evil.example/v1");
            record.context_window = 1;
            record
        };
        let changes = vec![CatalogChange::UpsertModelRecord {
            provider_id: "openai".into(),
            record: Box::new(replacement),
        }];
        let ProposalOutcome::Changes { lines, summary } =
            build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines[0].contains("API key destination changes"));
        assert!(summary.contains("baseUrl host changes"));
    }

    #[test]
    fn a_record_for_an_undefined_provider_is_rejected_unless_the_batch_defines_it() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let changes = vec![change("ghost", "ghost-1", "https://ghost.example/v1")];
        assert!(
            build_proposal(&providers, &changes)
                .unwrap_err()
                .contains("unknown provider")
        );

        let provider = CustomProvider {
            id: "ghost".to_string(),
            name: "Ghost".to_string(),
            base_url: "https://ghost.example/v1".to_string(),
            default_api: "openai-completions".to_string(),
        };
        let changes = vec![
            CatalogChange::UpsertCustomProvider {
                provider: provider.clone(),
            },
            change("ghost", "ghost-1", "https://ghost.example/v1"),
        ];
        let ProposalOutcome::Changes { lines, .. } = build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines.iter().any(|line| line.contains("+ provider ghost")));
        apply_changes(&providers, &providers.settings.snapshot(), &changes).unwrap();
        assert!(providers.is_eligible("ghost"));
        assert!(providers.has_model("ghost", "ghost-1"));
    }

    #[test]
    fn hidden_changes_diff_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let first = providers.models_for("openai")[0].clone();
        let bare = first.id.strip_prefix("openai/").unwrap().to_string();

        let mut ids = BTreeSet::new();
        ids.insert(bare.clone());
        let changes = vec![CatalogChange::SetHiddenModels {
            provider_id: "openai".into(),
            model_ids: ids.clone(),
        }];
        let ProposalOutcome::Changes { lines, .. } = build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines[0].contains("hide"));

        apply_changes(&providers, &providers.settings.snapshot(), &changes).unwrap();
        assert!(
            !providers
                .models_for("openai")
                .iter()
                .any(|row| row.id == first.id)
        );

        // Re-proposing the same set: no-op. Unhiding (empty set) works.
        let ProposalOutcome::NoChanges { .. } = build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a no-op");
        };
        let unhide = vec![CatalogChange::SetHiddenModels {
            provider_id: "openai".into(),
            model_ids: BTreeSet::new(),
        }];
        let ProposalOutcome::Changes { lines, .. } = build_proposal(&providers, &unhide).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines[0].contains("unhide"));
        apply_changes(&providers, &providers.settings.snapshot(), &unhide).unwrap();
        assert!(
            providers
                .models_for("openai")
                .iter()
                .any(|row| row.id == first.id)
        );
    }

    #[test]
    fn the_record_dump_is_the_replacement_template() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let first = providers.models_for("openai")[0].clone();
        let bare = first.id.strip_prefix("openai/").unwrap().to_string();

        // The dump round-trips: its JSON parses back into the record the
        // catalog serves, qualified or bare id alike.
        let dump_checks = [first.id.clone(), bare.clone()];
        for id in dump_checks {
            let lines = record_dump(&providers, "openai", &id).unwrap();
            assert!(lines[0].contains("hidden: false"));
            let dumped: CoreModel = serde_json::from_str(&lines[1]).unwrap();
            assert_eq!(
                dumped,
                providers.resolve_model("openai", &first.id).unwrap()
            );
        }

        // Hidden models still dump (they stay resolvable), unknown ids error.
        let mut hidden = std::collections::BTreeSet::new();
        hidden.insert(bare.clone());
        providers
            .settings
            .set_hidden_models("openai", hidden)
            .unwrap();
        let lines = record_dump(&providers, "openai", &bare).unwrap();
        assert!(lines[0].contains("hidden: true"));
        assert!(record_dump(&providers, "openai", "gpt-nope").is_err());
    }

    #[test]
    fn a_record_with_headers_dumps_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let mut headed = record("openai", "gpt-headed", "https://api.openai.com/v1");
        headed.headers = Some(std::collections::BTreeMap::from([(
            "X-Api-Key".to_string(),
            "secret-value".to_string(),
        )]));
        providers
            .settings
            .upsert_model_record("openai", headed)
            .unwrap();

        let lines = record_dump(&providers, "openai", "gpt-headed").unwrap();
        assert!(!lines.join("\n").contains("secret-value"));
        let json_index = lines
            .iter()
            .position(|line| line.trim_start().starts_with('{'))
            .expect("the dump still carries the record JSON");
        let dumped: CoreModel = serde_json::from_str(&lines[json_index]).unwrap();
        assert!(dumped.headers.is_none());
    }

    #[test]
    fn a_replacement_proposal_keeps_the_stored_headers() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let mut headed = record("openai", "gpt-headed", "https://api.openai.com/v1");
        headed.headers = Some(std::collections::BTreeMap::from([(
            "X-Api-Key".to_string(),
            "secret-value".to_string(),
        )]));
        providers
            .settings
            .upsert_model_record("openai", headed)
            .unwrap();

        // A bare replacement (parse_change rejects headers, so proposals
        // can never carry them) differs only in its context window.
        let mut replacement = record("openai", "gpt-headed", "https://api.openai.com/v1");
        replacement.context_window = 111_222;
        let changes = vec![CatalogChange::UpsertModelRecord {
            provider_id: "openai".to_string(),
            record: Box::new(replacement),
        }];

        let ProposalOutcome::Changes { lines, .. } = build_proposal(&providers, &changes).unwrap()
        else {
            panic!("expected a change");
        };
        assert!(lines[0].contains("contextWindow"));
        assert!(!lines.join("\n").contains("headers"));

        apply_changes(&providers, &providers.settings.snapshot(), &changes).unwrap();
        let applied = providers
            .settings
            .model_records_for("openai")
            .into_iter()
            .find(|stored| stored.id == "gpt-headed")
            .unwrap();
        assert_eq!(applied.context_window, 111_222);
        assert_eq!(
            applied
                .headers
                .as_ref()
                .and_then(|headers| headers.get("X-Api-Key"))
                .map(String::as_str),
            Some("secret-value")
        );
    }

    #[test]
    fn a_planned_probe_needs_the_exact_approved_triple() {
        let approved =
            HashSet::from([("ghost".to_string(), "https://ghost.example/v1".to_string())]);
        // The exact (provider, baseUrl) the card showed: key rides.
        assert!(planned_probe_key_allowed(
            &approved,
            "ghost",
            "https://ghost.example/v1"
        ));
        // A different baseUrl re-arms — no key until a new Key request
        // settles for the new destination.
        assert!(!planned_probe_key_allowed(
            &approved,
            "ghost",
            "https://evil.example/v1"
        ));
        // String-exact, on purpose: the approval is the URL the user saw.
        assert!(!planned_probe_key_allowed(
            &approved,
            "ghost",
            "https://ghost.example/v1/"
        ));
        // Another provider's approval never carries over.
        assert!(!planned_probe_key_allowed(
            &approved,
            "other",
            "https://ghost.example/v1"
        ));
        // No approvals: keyless, exactly as before ADR-0031.
        let empty = HashSet::new();
        assert!(!planned_probe_key_allowed(
            &empty,
            "ghost",
            "https://ghost.example/v1"
        ));
    }

    #[test]
    fn listing_parsers_stay_tolerant() {
        assert_eq!(
            parse_model_listing(&serde_json::json!({ "data": [{ "id": "a" }, { "id": "b" }] })),
            vec!["a", "b"]
        );
        assert_eq!(
            parse_model_listing(&serde_json::json!({ "models": ["x", "y"] })),
            vec!["x", "y"]
        );
        assert_eq!(
            parse_model_listing(&serde_json::json!([{ "name": "z" }])),
            vec!["z"]
        );
        assert!(parse_model_listing(&serde_json::json!({ "error": true })).is_empty());
    }

    #[tokio::test]
    async fn planned_probe_targets_must_be_public() {
        // Public hosts pass — IP literals classify directly; the hostname
        // path is exercised by "localhost" below through /etc/hosts, so no
        // test here needs the network.
        for url in ["http://8.8.8.8/v1", "https://93.184.216.34/v1"] {
            assert!(
                planned_probe_problem(url).await.is_none(),
                "{url} should be probeable"
            );
        }
        // Everything a local or internal probe could reach is refused,
        // cloud metadata included (it lives in link-local space).
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
            "http://[::ffff:127.0.0.1]/v1",
            "http://10.0.0.5/v1",
            "http://172.16.0.1/v1",
            "http://192.168.1.10/v1",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/v1",
            "http://0.0.0.0/v1",
            "http://[fe80::1]/v1",
        ] {
            assert!(
                planned_probe_problem(url).await.is_some(),
                "{url} should be refused"
            );
        }
        assert!(planned_probe_problem("ftp://example.com").await.is_some());
    }

    fn draft(id: &str, base_url: &str) -> CustomProvider {
        CustomProvider {
            id: id.into(),
            name: format!("{id} draft"),
            base_url: base_url.into(),
            default_api: "openai-completions".into(),
        }
    }

    #[test]
    fn a_draft_probe_carries_the_key_only_for_its_approved_base_url() {
        let approved = HashSet::from([("acme".to_string(), "https://8.8.8.8/v1".to_string())]);
        let (target, key) =
            inquiry_probe_plan(Some(&draft("acme", "https://8.8.8.8/v1")), &approved);
        assert_eq!(
            target,
            Some((
                "https://8.8.8.8/v1".to_string(),
                "openai-completions".to_string()
            ))
        );
        assert!(key);
        let (_, key) = inquiry_probe_plan(Some(&draft("acme", "https://1.1.1.1/v1")), &approved);
        assert!(!key, "a different baseUrl re-arms");
        // No draft: the stored target with the stored key, as before.
        assert_eq!(inquiry_probe_plan(None, &approved), (None, true));
    }

    #[test]
    fn drafts_parse_and_validate() {
        assert_eq!(parse_draft(&serde_json::json!({})).unwrap(), None);
        let ok = parse_draft(&serde_json::json!({ "provider": {
            "id": "acme", "name": "Acme", "baseUrl": "https://8.8.8.8/v1",
            "defaultApi": "openai-completions",
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(ok.id, "acme");
        assert!(parse_draft(&serde_json::json!({ "provider": { "id": "acme" } })).is_err());
        assert!(
            parse_draft(&serde_json::json!({ "provider": {
                "id": "acme", "name": "Acme", "baseUrl": "http://acme.example/v1",
                "defaultApi": "openai-completions",
            }}))
            .is_err(),
            "plaintext http off loopback"
        );
    }

    #[tokio::test]
    async fn a_key_request_falls_back_to_a_public_draft() {
        let dir = tempfile::tempdir().unwrap();
        let providers = adapter(dir.path());
        let chat = ChatRuntime::new();
        // Unknown and no draft: refused.
        assert!(
            key_request_target(&providers, &chat, "acme", None)
                .await
                .is_err()
        );
        // A public draft addresses the card.
        let request = key_request_target(
            &providers,
            &chat,
            "acme",
            Some(draft("acme", "https://8.8.8.8/v1")),
        )
        .await
        .unwrap();
        assert_eq!(request.destination, "https://8.8.8.8/v1");
        assert_eq!(request.provider_name, "acme draft");
        // A loopback draft is refused by the public-host gate.
        assert!(
            key_request_target(
                &providers,
                &chat,
                "acme",
                Some(draft("acme", "http://127.0.0.1:8080/v1")),
            )
            .await
            .is_err()
        );
        // A mismatched id is refused.
        assert!(
            key_request_target(
                &providers,
                &chat,
                "acme",
                Some(draft("other", "https://8.8.8.8/v1")),
            )
            .await
            .is_err()
        );
        // A catalog provider resolves first; the draft cannot redirect it.
        let request = key_request_target(
            &providers,
            &chat,
            "openai",
            Some(draft("openai", "https://8.8.8.8/v1")),
        )
        .await
        .unwrap();
        assert_ne!(request.destination, "https://8.8.8.8/v1");
    }
}
