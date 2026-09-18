use std::{collections::HashMap, collections::HashSet, sync::Arc};

use holt_proto::{
    Model as HoltModel, Provider as HoltProvider, ProviderId, ProviderVariant, ReasoningLevel,
};
use pi_core::ai::types::{Model as CoreModel, ModelInput};

use crate::{
    credentials::HoltCredentialStore,
    provider_settings::ProviderSettingsStore,
    provider_store::{Catalog, CatalogProvider},
};

pub struct ProviderAdapter {
    pub credentials: Arc<HoltCredentialStore>,
    pub settings: Arc<ProviderSettingsStore>,
    /// The boot-time merge of the compiled catalog and
    /// `provider-store.json`: every provider/model answer below reads it,
    /// never `builtin_providers()` ad hoc. Built in
    /// `LocalEngine::assemble`; a file edit needs a restart.
    catalog: Catalog,
}

impl ProviderAdapter {
    pub(crate) fn new(
        credentials: Arc<HoltCredentialStore>,
        settings: Arc<ProviderSettingsStore>,
        catalog: Catalog,
    ) -> Self {
        Self {
            credentials,
            settings,
            catalog,
        }
    }

    /// One catalog row per organization (single-provider rows included), in
    /// first-seen builtin order. Sibling providers sharing an
    /// `organization_id` collapse into one row whose `id` is the organization
    /// key; each row's variants carry the concrete, RPC-addressable ids.
    pub async fn providers(&self) -> Vec<HoltProvider> {
        let configured: HashSet<String> = self
            .credentials
            .configured_ids()
            .await
            .into_iter()
            .collect();
        let mut rows: Vec<HoltProvider> = Vec::new();
        let mut row_index: HashMap<String, usize> = HashMap::new();
        for provider in eligible_providers(&self.catalog) {
            let variant = ProviderVariant {
                id: ProviderId(provider.id.clone()),
                name: provider.name.clone(),
                configured: configured.contains(&provider.id),
            };
            let org_key = provider
                .organization_id
                .clone()
                .unwrap_or_else(|| provider.id.clone());
            match row_index.get(&org_key) {
                Some(&index) => {
                    if variant.configured {
                        rows[index].configured = true;
                    }
                    rows[index].variants.push(variant);
                }
                None => {
                    row_index.insert(org_key.clone(), rows.len());
                    rows.push(HoltProvider {
                        id: ProviderId(org_key),
                        name: provider.name.clone(),
                        abbreviation: abbreviation(&provider.name, &provider.id),
                        configured: variant.configured,
                        variants: vec![variant],
                        custom: false,
                    });
                }
            }
        }
        // User-defined providers stand alone — one row each, after the
        // built-ins. A definition colliding with a catalog id is inert: the
        // write path rejects it, and a hand-edited file loses it.
        for provider in self.settings.custom_providers() {
            if self.catalog.get(&provider.id).is_some() {
                continue;
            }
            let configured = configured.contains(&provider.id);
            rows.push(HoltProvider {
                id: ProviderId(provider.id.clone()),
                name: provider.name.clone(),
                abbreviation: abbreviation(&provider.name, &provider.id),
                configured,
                variants: vec![ProviderVariant {
                    id: ProviderId(provider.id.clone()),
                    name: provider.name.clone(),
                    configured,
                }],
                custom: true,
            });
        }
        rows
    }

    /// Every window the engine can divide by, keyed by wire id
    /// (`provider/model`) — the whole eligible-provider catalog plus every
    /// user-defined provider, with metadata-opaque rows `None` by contract.
    /// The usage frame's occupancy denominator is looked up here, so nothing
    /// is cached per chat: a builtin window never moves within a process.
    /// Hidden models keep their window — hiding is a listing concern, and a
    /// chat already running one still needs its denominator.
    pub fn context_windows(&self) -> Vec<(String, Option<u64>)> {
        let mut windows = Vec::new();
        for provider_id in self.effective_provider_ids() {
            let (opaque_ids, custom_ids) = self.layer_ids(&provider_id);
            windows.extend(
                project_models(self.core_models_for(&provider_id), &opaque_ids, &custom_ids)
                    .into_iter()
                    .map(|model| (model.id, model.context_window)),
            );
        }
        windows
    }

    pub fn models_for(&self, provider_id: &str) -> Vec<HoltModel> {
        let (opaque_ids, custom_ids) = self.layer_ids(provider_id);
        let hidden: HashSet<String> = self
            .settings
            .hidden_models_for(provider_id)
            .into_iter()
            .collect();
        let shown: Vec<CoreModel> = self
            .core_models_for(provider_id)
            .into_iter()
            .filter(|model| !hidden.contains(&model.id))
            .collect();
        project_models(shown, &opaque_ids, &custom_ids)
    }

    pub fn resolve_model(
        &self,
        provider_id: &str,
        qualified_id: &str,
    ) -> Result<CoreModel, String> {
        let (qualified_provider, model_id) = qualified_id
            .split_once('/')
            .ok_or_else(|| format!("model must use provider/model syntax: {qualified_id}"))?;
        if qualified_provider != provider_id {
            return Err("provider and model do not match".into());
        }
        self.core_models_for(provider_id)
            .into_iter()
            .find(|model| model.id == model_id)
            .ok_or_else(|| format!("unknown model: {qualified_id}"))
    }

    pub fn can_add_custom_model(&self, provider_id: &str) -> bool {
        !self.core_models_for(provider_id).is_empty()
    }

    /// Is `model_id` already in the provider's model list — the merged
    /// catalog, a live record, or a user-added custom id? Additions must be
    /// new ids.
    pub fn has_model(&self, provider_id: &str, model_id: &str) -> bool {
        self.core_models_for(provider_id)
            .iter()
            .any(|model| model.id == model_id)
    }

    pub fn is_eligible(&self, provider_id: &str) -> bool {
        eligible_providers(&self.catalog)
            .iter()
            .any(|provider| provider.id == provider_id)
            || (self.catalog.get(provider_id).is_none()
                && self.settings.custom_provider(provider_id).is_some())
    }

    /// Write-path validation for a live model record: the provider must be
    /// one the engine addresses, and the record must be servable under the
    /// same rules the provider store applies to its file entries.
    pub fn validate_model_record(
        &self,
        provider_id: &str,
        record: &CoreModel,
    ) -> Result<(), String> {
        if self.catalog.get(provider_id).is_none()
            && self.settings.custom_provider(provider_id).is_none()
        {
            return Err("unknown provider".into());
        }
        if let Some(problem) = crate::provider_store::model_record_problem(provider_id, record) {
            return Err(format!("model record is not servable: {problem}"));
        }
        Ok(())
    }

    /// Write-path validation for a user-defined provider: structural rules
    /// plus the reserved-id check against the boot catalog.
    pub fn validate_custom_provider(
        &self,
        provider: &crate::provider_settings::CustomProvider,
    ) -> Result<(), String> {
        if let Some(problem) = crate::provider_settings::custom_provider_problem(provider) {
            return Err(format!("custom provider is not servable: {problem}"));
        }
        if self.catalog.get(&provider.id).is_some() {
            return Err("provider id already exists in the built-in catalog".into());
        }
        Ok(())
    }

    /// Is this provider id one the engine addresses — a boot-catalog id
    /// (compiled or overlaid) or a user-defined provider? Live records and
    /// the model-setup tools key off this.
    pub(crate) fn provider_known(&self, provider_id: &str) -> bool {
        self.catalog.get(provider_id).is_some()
            || self.settings.custom_provider(provider_id).is_some()
    }

    pub(crate) fn catalog_has_provider(&self, provider_id: &str) -> bool {
        self.catalog.get(provider_id).is_some()
    }

    pub(crate) fn catalog_has_model(&self, provider_id: &str, model_id: &str) -> bool {
        self.catalog
            .models(provider_id)
            .iter()
            .any(|model| model.id == model_id)
    }

    /// The eligible built-ins plus the user-defined providers, in that
    /// order; a custom definition colliding with a catalog id never wins.
    fn effective_provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = eligible_providers(&self.catalog)
            .iter()
            .map(|provider| provider.id.clone())
            .collect();
        for provider in self.settings.custom_providers() {
            if !ids.contains(&provider.id) && self.catalog.get(&provider.id).is_none() {
                ids.push(provider.id);
            }
        }
        ids
    }

    /// The provider's two settings-layer id sets: `opaque` — legacy bare
    /// custom ids that shadow nothing (rejected at add time for catalog
    /// ids; live records shadow them by design) — and every id the user
    /// owns (`opaque` plus record ids). Opaque rows borrow a template's
    /// transport and keep their metadata withheld; record rows carry their
    /// own.
    fn layer_ids(&self, provider_id: &str) -> (HashSet<String>, HashSet<String>) {
        let catalog: HashSet<String> = self
            .catalog
            .models(provider_id)
            .iter()
            .map(|model| model.id.clone())
            .collect();
        let records: HashSet<String> = self
            .settings
            .model_records_for(provider_id)
            .iter()
            .map(|model| model.id.clone())
            .collect();
        let opaque: HashSet<String> = self
            .settings
            .custom_models_for(provider_id)
            .into_iter()
            .filter(|id| !catalog.contains(id) && !records.contains(id))
            .collect();
        let mut custom = records;
        custom.extend(opaque.iter().cloned());
        (opaque, custom)
    }

    fn core_models_for(&self, provider_id: &str) -> Vec<CoreModel> {
        // Live records apply only to providers the engine addresses: boot
        // catalog ids (compiled or overlaid) and user-defined providers.
        // Records filed under any other id are inert.
        if !self.provider_known(provider_id) {
            return Vec::new();
        }
        let mut models = self.catalog.models(provider_id).to_vec();
        // The top catalog layer (ADR-0028): a record replaces the same-id
        // entry outright — complete records, so per-field merging is not
        // expressible — and a new id appends.
        for record in self.settings.model_records_for(provider_id) {
            match models.iter_mut().find(|model| model.id == record.id) {
                Some(slot) => *slot = record,
                None => models.push(record),
            }
        }
        let Some(template) = models.first().cloned() else {
            return models;
        };
        for custom_id in self.settings.custom_models_for(provider_id) {
            if models.iter().any(|model| model.id == custom_id) {
                continue;
            }
            let mut model = template.clone();
            model.id = custom_id.clone();
            model.name = custom_id;
            model.reasoning = false;
            model.thinking_level_map = None;
            model.cost = Default::default();
            // Unknown custom ids may attempt image input. This is transport
            // policy only; the catalog projection still reports Unknown.
            if !model.input.contains(&ModelInput::Image) {
                model.input.push(ModelInput::Image);
            }
            models.push(model);
        }
        models
    }
}

/// Holt's availability policy, as a filter over the boot-time catalog rather
/// than a rebuilt list: `api_key` auth shaped, complex-auth providers
/// excluded.
fn eligible_providers(catalog: &Catalog) -> Vec<&CatalogProvider> {
    catalog
        .iter()
        .filter(|provider| provider.auth.api_key)
        .filter(|provider| !complex_auth_provider(&provider.id))
        .collect()
}

fn complex_auth_provider(id: &str) -> bool {
    [
        "amazon-bedrock",
        "azure-openai",
        "cloudflare-",
        "google-vertex",
        "radius",
    ]
    .iter()
    .any(|prefix| id == *prefix || id.starts_with(prefix))
}

fn abbreviation(name: &str, id: &str) -> String {
    let initials: String = name
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.chars().next())
        .take(3)
        .flat_map(char::to_uppercase)
        .collect();
    if initials.len() >= 2 {
        return initials;
    }
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(3)
        .flat_map(char::to_uppercase)
        .collect()
}

fn project_models(
    mut models: Vec<CoreModel>,
    opaque_ids: &HashSet<String>,
    custom_ids: &HashSet<String>,
) -> Vec<HoltModel> {
    models.retain(|model| custom_ids.contains(&model.id) || !dated_snapshot(&model.id));
    models.sort_by_key(|model| (model.id.ends_with("-latest"), model.id.clone()));

    let mut seen_names = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut projected = Vec::new();
    for model in models {
        let canonical_id = model.id.strip_suffix("-latest").unwrap_or(&model.id);
        let name_key = model.name.to_ascii_lowercase();
        if seen_ids.contains(canonical_id) || !seen_names.insert(name_key) {
            continue;
        }
        seen_ids.insert(canonical_id.to_string());
        let reasoning_levels = if model.reasoning {
            vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ]
        } else {
            Vec::new()
        };
        let custom = custom_ids.contains(&model.id);
        projected.push(HoltModel {
            id: format!("{}/{}", model.provider, model.id),
            provider: ProviderId(model.provider.clone()),
            label: model.name,
            description: Some(model.provider),
            default_reasoning: model.reasoning.then_some(ReasoningLevel::High),
            reasoning_levels,
            options: Vec::new(),
            custom,
            // A legacy bare id's window came along with the cloned builtin
            // template — a guess, so it is withheld (unknown beats a
            // fabricated percentage). A live record's window is its own.
            context_window: if opaque_ids.contains(&model.id) {
                None
            } else {
                Some(model.context_window)
            },
            image_capability: if opaque_ids.contains(&model.id) {
                holt_proto::ImageCapability::Unknown
            } else if model.input.contains(&ModelInput::Image) {
                holt_proto::ImageCapability::Supported
            } else {
                holt_proto::ImageCapability::Unsupported
            },
        });
    }
    projected
}

fn dated_snapshot(id: &str) -> bool {
    id.split(['-', '_', '.']).any(|part| {
        (part.len() == 8 || part.len() == 6)
            && part.chars().all(|character| character.is_ascii_digit())
    }) || id.as_bytes().windows(10).any(|window| {
        window[0..4].iter().all(u8::is_ascii_digit)
            && window[4] == b'-'
            && window[5..7].iter().all(u8::is_ascii_digit)
            && window[7] == b'-'
            && window[8..10].iter().all(u8::is_ascii_digit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_settings::CustomProvider;
    use std::path::Path;

    fn adapter(data_dir: &Path) -> ProviderAdapter {
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
            "input": ["text", "image"],
            "cost": { "input": 1.5, "output": 3.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
            "contextWindow": 321_000,
            "maxTokens": 16_384,
        }))
        .unwrap()
    }

    #[test]
    fn a_record_replaces_in_place_and_appends_with_first_class_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let first_compiled = adapter.models_for("openai")[0].clone();
        let replaced_id = first_compiled
            .id
            .strip_prefix("openai/")
            .unwrap()
            .to_string();

        let mut replacement = record("openai", &replaced_id, "https://proxy.example/v1");
        replacement.input = vec![ModelInput::Image, ModelInput::Text];
        adapter
            .settings
            .upsert_model_record("openai", replacement)
            .unwrap();
        adapter
            .settings
            .upsert_model_record(
                "openai",
                record("openai", "gpt-via-record", "https://api.openai.com/v1"),
            )
            .unwrap();

        let rows = adapter.models_for("openai");
        let replaced = rows.iter().find(|row| row.id == first_compiled.id).unwrap();
        assert_eq!(replaced.context_window, Some(321_000));
        assert_eq!(
            replaced.image_capability,
            holt_proto::ImageCapability::Supported
        );
        assert!(replaced.custom);
        let appended = rows
            .iter()
            .find(|row| row.id == "openai/gpt-via-record")
            .unwrap();
        assert_eq!(appended.context_window, Some(321_000));
        assert!(appended.custom);

        // Resolution hands the transport the record, not the compiled entry
        // it replaced — the metadata fix rides the request path.
        let resolved = adapter
            .resolve_model("openai", "openai/gpt-via-record")
            .unwrap();
        assert_eq!(resolved.base_url, "https://api.openai.com/v1");
        assert_eq!(resolved.cost.rates.input.0, 1.5);
    }

    #[test]
    fn legacy_ids_stay_opaque_and_records_shadow_them() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        adapter
            .settings
            .add_custom_model("openai", "gpt-bare")
            .unwrap();
        let rows = adapter.models_for("openai");
        let bare = rows.iter().find(|row| row.id == "openai/gpt-bare").unwrap();
        assert_eq!(bare.context_window, None);
        assert_eq!(bare.image_capability, holt_proto::ImageCapability::Unknown);

        // A record under the same id is strictly richer: it wins, and the
        // row stops being opaque.
        adapter
            .settings
            .upsert_model_record(
                "openai",
                record("openai", "gpt-bare", "https://api.openai.com/v1"),
            )
            .unwrap();
        let rows = adapter.models_for("openai");
        let shadowed = rows.iter().find(|row| row.id == "openai/gpt-bare").unwrap();
        assert_eq!(shadowed.context_window, Some(321_000));
    }

    #[test]
    fn hidden_models_leave_listings_but_not_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let first = adapter.models_for("openai")[0].clone();
        let bare_id = first.id.strip_prefix("openai/").unwrap().to_string();

        let mut hidden = std::collections::BTreeSet::new();
        hidden.insert(bare_id.clone());
        adapter
            .settings
            .set_hidden_models("openai", hidden)
            .unwrap();

        assert!(
            adapter
                .models_for("openai")
                .iter()
                .all(|row| row.id != first.id)
        );
        assert!(adapter.has_model("openai", &bare_id));
        assert!(adapter.resolve_model("openai", &first.id).is_ok());
        // The occupancy table keeps the denominator a running chat needs.
        assert!(
            adapter
                .context_windows()
                .iter()
                .any(|(id, window)| id == &first.id && window.is_some())
        );
    }

    #[test]
    fn custom_providers_get_their_own_row_and_key_eligibility() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let provider = CustomProvider {
            id: "acme".to_string(),
            name: "Acme Gateway".to_string(),
            base_url: "https://acme.example/v1".to_string(),
            headers: None,
            default_api: "openai-completions".to_string(),
        };
        adapter.validate_custom_provider(&provider).unwrap();
        adapter.settings.upsert_custom_provider(provider).unwrap();
        adapter
            .settings
            .upsert_model_record("acme", record("acme", "acme-1", "https://acme.example/v1"))
            .unwrap();

        let rows = futures::executor::block_on(adapter.providers());
        let row = rows.iter().find(|row| row.id.0 == "acme").unwrap();
        assert_eq!(row.name, "Acme Gateway");
        assert_eq!(row.variants.len(), 1);
        assert!(!row.configured);

        assert!(adapter.is_eligible("acme"));
        assert!(adapter.can_add_custom_model("acme"));
        let models = adapter.models_for("acme");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "acme/acme-1");
        assert_eq!(models[0].context_window, Some(321_000));
        // The record is the template a later bare id borrows.
        adapter
            .settings
            .add_custom_model("acme", "acme-bare")
            .unwrap();
        assert!(adapter.has_model("acme", "acme-bare"));
    }

    #[test]
    fn a_custom_provider_colliding_with_a_catalog_id_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = adapter(dir.path());
        let provider = CustomProvider {
            id: "openai".to_string(),
            name: "Hostile Twin".to_string(),
            base_url: "https://evil.example/v1".to_string(),
            headers: None,
            default_api: "openai-completions".to_string(),
        };
        // The write path rejects it…
        assert!(adapter.validate_custom_provider(&provider).is_err());
        // …and a hand-planted definition changes nothing: no row, no
        // eligibility change, no provider rename.
        adapter.settings.upsert_custom_provider(provider).unwrap();
        let rows = futures::executor::block_on(adapter.providers());
        let openai_rows = rows
            .iter()
            .filter(|row| row.variants.iter().any(|variant| variant.id.0 == "openai"))
            .count();
        assert_eq!(openai_rows, 1);
        assert!(rows.iter().all(|row| row.name != "Hostile Twin"));
    }

    #[test]
    fn excludes_complex_and_oauth_only_providers() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = crate::provider_store::load(dir.path());
        let ids: HashSet<String> = eligible_providers(&catalog)
            .into_iter()
            .map(|p| p.id.clone())
            .collect();
        assert!(ids.contains("openai"));
        assert!(ids.contains("anthropic"));
        assert!(!ids.iter().any(|id| id.contains("bedrock")
            || id.contains("vertex")
            || id.contains("azure")
            || id.contains("cloudflare")));
    }

    #[test]
    fn snapshot_detection_is_narrow() {
        assert!(dated_snapshot("claude-3-5-sonnet-20241022"));
        assert!(dated_snapshot("model-2024-10-22"));
        assert!(!dated_snapshot("gpt-5.4"));
    }
}
