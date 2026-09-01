use std::{collections::HashSet, sync::Arc};

use holt_proto::{Model as HoltModel, Provider as HoltProvider, ProviderId, ReasoningLevel};
use pi_core::ai::{
    models::{CreateModelsOptions, Models},
    providers::builtin::{builtin_models, builtin_providers},
    types::Model as CoreModel,
};

use crate::credentials::HoltCredentialStore;

pub struct ProviderAdapter {
    pub models: Arc<Models>,
    pub credentials: Arc<HoltCredentialStore>,
}

impl ProviderAdapter {
    pub fn new(credentials: Arc<HoltCredentialStore>) -> Self {
        let models = builtin_models(CreateModelsOptions {
            credentials: Some(credentials.clone()),
            ..Default::default()
        });
        Self {
            models,
            credentials,
        }
    }

    pub async fn providers(&self) -> Vec<HoltProvider> {
        let configured: HashSet<String> = self
            .credentials
            .configured_ids()
            .await
            .into_iter()
            .collect();
        eligible_providers()
            .into_iter()
            .map(|provider| HoltProvider {
                id: ProviderId(provider.id().to_string()),
                name: provider.name().to_string(),
                abbreviation: abbreviation(provider.name(), provider.id()),
                configured: configured.contains(provider.id()),
            })
            .collect()
    }

    pub fn models_for(&self, provider_id: &str) -> Vec<HoltModel> {
        project_models(self.models.get_models(Some(provider_id)))
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
        self.models
            .get_model(provider_id, model_id)
            .ok_or_else(|| format!("unknown model: {qualified_id}"))
    }

    pub fn is_eligible(provider_id: &str) -> bool {
        eligible_providers()
            .iter()
            .any(|provider| provider.id() == provider_id)
    }
}

fn eligible_providers() -> Vec<Arc<dyn pi_core::ai::models::Provider>> {
    builtin_providers()
        .into_iter()
        .filter(|provider| provider.auth().api_key.is_some())
        .filter(|provider| !complex_auth_provider(provider.id()))
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

fn project_models(mut models: Vec<CoreModel>) -> Vec<HoltModel> {
    models.retain(|model| !dated_snapshot(&model.id));
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
        projected.push(HoltModel {
            id: format!("{}/{}", model.provider, model.id),
            provider: ProviderId(model.provider.clone()),
            label: model.name,
            description: Some(model.provider),
            default_reasoning: model.reasoning.then_some(ReasoningLevel::High),
            reasoning_levels,
            options: Vec::new(),
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

    #[test]
    fn excludes_complex_and_oauth_only_providers() {
        let ids: HashSet<String> = eligible_providers()
            .into_iter()
            .map(|p| p.id().to_string())
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
