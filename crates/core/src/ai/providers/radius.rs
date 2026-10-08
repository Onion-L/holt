//! Port of `pi-core/ai/src/providers/radius.ts`: the Radius gateway provider
//! with a persisted, dynamically refreshed catalog. Unlike the catalogue
//! providers this is a hand-rolled `Provider`, not `createProvider`.

use std::sync::{Arc, Mutex};

use crate::ai::auth::helpers::env_api_key_auth;
use crate::ai::auth::oauth::radius::{RadiusOAuthOptions, create_radius_oauth};
use crate::ai::auth::resolve::{ModelsError, ModelsErrorCode};
use crate::ai::auth::types::{Credential, ProviderAuth};
use crate::ai::models::{ModelsPublication, Provider, ProviderStreams, RefreshModelsContext};
use crate::ai::models_store::ModelsStoreEntry;
use crate::ai::providers::apis::pi_messages_api;
use crate::ai::providers::radius_config::{
    DEFAULT_RADIUS_GATEWAY, RadiusGatewayConfig, get_radius_models, get_radius_models_from_config,
    load_radius_gateway_config, normalize_radius_gateway_url,
};
use crate::ai::types::Model;
use crate::ai::utils::http::HttpFetch;
use futures::future::BoxFuture;

/// Port of `RadiusProviderOptions`. The `fetch` field is the Rust injection
/// seam for the TypeScript global `fetch` used by `loadRadiusGatewayConfig`.
#[derive(Default)]
pub struct RadiusProviderOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub gateway: Option<String>,
    pub fetch: Option<Arc<dyn HttpFetch>>,
}

struct RadiusProvider {
    id: String,
    name: String,
    gateway: String,
    auth: ProviderAuth,
    models: Arc<Mutex<Vec<Model>>>,
    streams: Arc<dyn ProviderStreams>,
    fetch: Option<Arc<dyn HttpFetch>>,
}

/// Port of `radiusProvider`.
pub fn radius_provider(options: RadiusProviderOptions) -> Arc<dyn Provider> {
    let id = options.id.unwrap_or_else(|| "radius".to_string());
    let name = options.name.unwrap_or_else(|| "Radius".to_string());
    let gateway =
        normalize_radius_gateway_url(options.gateway.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY));
    let auth = ProviderAuth {
        api_key: Some(env_api_key_auth("Radius API key", &["RADIUS_API_KEY"])),
        oauth: Some(create_radius_oauth(RadiusOAuthOptions {
            name: name.clone(),
            gateway: gateway.clone(),
        })),
    };
    let initial = get_radius_models(&id, None);
    Arc::new(RadiusProvider {
        id,
        name,
        gateway,
        auth,
        models: Arc::new(Mutex::new(initial)),
        streams: pi_messages_api(),
        fetch: options.fetch,
    })
}

impl RadiusProvider {
    async fn publish(
        publish: &Arc<
            dyn Fn(
                    ModelsPublication,
                )
                    -> BoxFuture<'static, Result<bool, crate::ai::models_store::ModelsStoreError>>
                + Send
                + Sync,
        >,
        publication: ModelsPublication,
        id: &str,
    ) -> Result<bool, ModelsError> {
        publish(publication).await.map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                format!("Model store write failed for {id}"),
                &error,
            )
        })
    }

    async fn load_config(
        &self,
        api_key: Option<&str>,
        signal: &tokio_util::sync::CancellationToken,
    ) -> Result<RadiusGatewayConfig, ModelsError> {
        load_radius_gateway_config(&self.gateway, api_key, Some(signal), self.fetch.clone())
            .await
            .map_err(|message| {
                ModelsError::new(
                    ModelsErrorCode::ModelSource,
                    format!("Model refresh failed for {}: {message}", self.id),
                )
            })
    }
}

impl Provider for RadiusProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn refresh_models<'a>(
        &'a self,
        context: RefreshModelsContext,
    ) -> BoxFuture<'a, Result<(), ModelsError>> {
        let RefreshModelsContext {
            credential,
            stored,
            publish,
            allow_network,
            signal,
            ..
        } = context;
        let models_state = Arc::clone(&self.models);
        let provider_id = self.id.clone();
        Box::pin(async move {
            if let Some(stored) = &stored {
                let restored: Vec<Model> = stored
                    .models
                    .iter()
                    .filter(|model| model.provider == provider_id)
                    .cloned()
                    .collect();
                let published = Self::publish(
                    &publish,
                    ModelsPublication {
                        persist: None,
                        update: Some(Box::new({
                            let restored = restored.clone();
                            let state = Arc::clone(&models_state);
                            move || {
                                *state
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) = restored;
                            }
                        })),
                    },
                    &provider_id,
                )
                .await?;
                if !published {
                    return Ok(());
                }
            }

            // Import catalogs cached by the pre-ModelsStore Radius
            // implementation.
            if stored.is_none()
                && let Some(Credential::OAuth(oauth)) = credential.as_ref()
            {
                let legacy = get_radius_models(&provider_id, Some(oauth));
                if !legacy.is_empty() {
                    let checked_at = crate::ai::auth::resolve::now_millis();
                    let published = Self::publish(
                        &publish,
                        ModelsPublication {
                            persist: Some(Some(ModelsStoreEntry {
                                models: legacy.clone(),
                                checked_at: Some(checked_at),
                                ..Default::default()
                            })),
                            update: Some(Box::new({
                                let state = Arc::clone(&models_state);
                                move || {
                                    *state
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        legacy;
                                }
                            })),
                        },
                        &provider_id,
                    )
                    .await?;
                    if !published {
                        return Ok(());
                    }
                }
            }

            if !allow_network || signal.is_cancelled() {
                return Ok(());
            }
            let api_key = match credential.as_ref() {
                Some(Credential::OAuth(oauth)) => Some(oauth.access.clone()),
                Some(Credential::ApiKey(api_key)) => api_key.key.clone(),
                None => None,
            };
            let config = self.load_config(api_key.as_deref(), &signal).await?;
            if signal.is_cancelled() {
                return Ok(());
            }
            let refreshed = get_radius_models_from_config(&provider_id, &config);
            let checked_at = crate::ai::auth::resolve::now_millis();
            Self::publish(
                &publish,
                ModelsPublication {
                    persist: Some(Some(ModelsStoreEntry {
                        models: refreshed.clone(),
                        checked_at: Some(checked_at),
                        ..Default::default()
                    })),
                    update: Some(Box::new({
                        let state = Arc::clone(&models_state);
                        move || {
                            *state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = refreshed;
                        }
                    })),
                },
                &provider_id,
            )
            .await?;
            Ok(())
        })
    }

    fn has_refresh_models(&self) -> bool {
        true
    }

    fn stream(
        &self,
        model: &Model,
        context: &crate::ai::types::Context,
        options: Option<&crate::ai::types::StreamOptions>,
    ) -> crate::ai::utils::event_stream::AssistantMessageEventStream {
        self.streams.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &crate::ai::types::Context,
        options: Option<&crate::ai::types::SimpleStreamOptions>,
    ) -> crate::ai::utils::event_stream::AssistantMessageEventStream {
        self.streams.stream_simple(model, context, options)
    }
}
