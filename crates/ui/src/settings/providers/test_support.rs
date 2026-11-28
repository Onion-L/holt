//! Shared test harness: a fake engine serving the providers page's reads
//! and writes, and the visual-window driver.

use super::*;

/// The fake engine behind the page tests: the providers/models reads and
/// the definition, key, and record writes (recorded for assertions).
pub(super) struct FakeProvidersEngine {
    /// The panels' catalog — the second entry is the user's custom
    /// model, dropped when `ResetProviderCatalog` lands.
    pub(super) catalog: std::sync::Mutex<Vec<serde_json::Value>>,
    /// SaveModelRecord params, in call order.
    pub(super) records: std::sync::Mutex<Vec<serde_json::Value>>,
    /// SaveCustomProvider params, in call order.
    pub(super) custom_saves: std::sync::Mutex<Vec<serde_json::Value>>,
    /// SaveProviderKey params, in call order.
    pub(super) saved_keys: std::sync::Mutex<Vec<serde_json::Value>>,
    /// ProbeProvider params, in call order.
    pub(super) probes: std::sync::Mutex<Vec<serde_json::Value>>,
    /// The canned ProbeProvider listing (bare ids, one already offered).
    pub(super) probe_ids: std::sync::Mutex<Vec<String>>,
    /// The provider-qualified ids `ListHiddenModels` reports.
    pub(super) hidden_ids: std::sync::Mutex<Vec<String>>,
    /// SetProviderLogo / RemoveProviderLogo calls: (method, params).
    pub(super) logo_calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    /// RemoveCustomProvider params, in call order.
    pub(super) removed: std::sync::Mutex<Vec<serde_json::Value>>,
    /// The store RevealProviderKey reads (provider → key).
    pub(super) keys: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Canned `PreviewFetchedModels` rows by id; an id without one gets the
    /// plain defaults the real engine answers for a model it has never
    /// heard of.
    pub(super) preview_rows: std::sync::Mutex<Vec<serde_json::Value>>,
    /// A complete raw `PreviewFetchedModels` reply, replacing the canned rows.
    pub(super) preview_reply: std::sync::Mutex<Option<serde_json::Value>>,
    /// SaveFetchedModels params, in call order.
    pub(super) fetched_saves: std::sync::Mutex<Vec<serde_json::Value>>,
    /// GetModelRecord's canned records, whole.
    pub(super) records_served: std::sync::Mutex<Vec<serde_json::Value>>,
    /// When set, `PreviewFetchedModels` fails with this message.
    pub(super) preview_error: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl holt_rpc::RpcService for FakeProvidersEngine {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
        use holt_rpc::{RpcError, RpcReply};
        match method {
            methods::LIST_PROVIDERS => {
                let provider = |id: &str, name: &str, abbreviation: &str, variants: &[&str]| {
                    serde_json::json!({
                        "id": id,
                        "name": name,
                        "abbreviation": abbreviation,
                        "configured": true,
                        "variants": variants
                            .iter()
                            .map(|variant| {
                                serde_json::json!({
                                    "id": variant,
                                    "name": name,
                                    "configured": true,
                                })
                            })
                            .collect::<Vec<_>>(),
                        "custom": false,
                    })
                };
                // Beta is an organization with a second (regional) variant.
                RpcReply::value(&serde_json::json!([
                    provider("acme", "Acme", "A", &["acme"]),
                    provider("beta", "Beta Labs", "B", &["beta", "beta-cn"]),
                ]))
            }
            methods::LIST_MODELS => {
                let provider = params["providerId"].as_str().unwrap_or_default();
                let rows: Vec<serde_json::Value> = self
                    .catalog
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|row| row["provider"] == serde_json::json!(provider))
                    .cloned()
                    .collect();
                RpcReply::value(&serde_json::Value::Array(rows))
            }
            methods::RESET_PROVIDER_CATALOG => {
                self.catalog
                    .lock()
                    .unwrap()
                    .retain(|row| row["id"] != "acme/acme-custom");
                RpcReply::value(&serde_json::json!({}))
            }
            methods::LIST_API_DIALECTS => RpcReply::value(&serde_json::json!([
                "anthropic-messages",
                "openai-completions",
                "openai-responses"
            ])),
            methods::SAVE_MODEL_RECORD => {
                self.records.lock().unwrap().push(params.clone());
                RpcReply::value(&serde_json::json!({}))
            }
            methods::PREVIEW_FETCHED_MODELS => {
                if let Some(error) = self.preview_error.lock().unwrap().clone() {
                    return Err(RpcError::Failed(error));
                }
                if let Some(reply) = self.preview_reply.lock().unwrap().clone() {
                    return RpcReply::value(&reply);
                }
                let canned = self.preview_rows.lock().unwrap().clone();
                let rows = params["modelIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|id| {
                        canned
                            .iter()
                            .find(|row| row["id"] == *id)
                            .cloned()
                            .unwrap_or_else(|| {
                                let id = id.as_str().unwrap_or_default();
                                serde_json::json!({
                                    "id": id,
                                    "name": match id.chars().next() {
                                        Some(first) => first.to_uppercase().collect::<String>() + &id[1..],
                                        None => String::new(),
                                    },
                                    "source": null,
                                    "startsFrom": null,
                                    "levelsEstimated": false,
                                    "estimatedFrom": null,
                                    "offered": false,
                                    "canTurnOff": true,
                                    "contextWindow": 200000,
                                    "maxTokens": 8192,
                                    "reasoningLevels": [],
                                    "input": ["text"],
                                    "cost": { "input": 0.0, "output": 0.0 },
                                })
                            })
                    })
                    .collect::<Vec<_>>();
                RpcReply::value(&serde_json::Value::Array(rows))
            }
            methods::SAVE_FETCHED_MODELS => {
                self.fetched_saves.lock().unwrap().push(params.clone());
                RpcReply::value(&serde_json::json!({}))
            }
            methods::GET_MODEL_RECORD => {
                let provider = params["providerId"].as_str().unwrap_or_default();
                let model = params["modelId"].as_str().unwrap_or_default();
                let served = self
                    .records_served
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|record| {
                        record["provider"] == provider
                            && (record["id"] == model
                                || format!(
                                    "{}/{}",
                                    record["provider"].as_str().unwrap_or_default(),
                                    record["id"].as_str().unwrap_or_default()
                                ) == model)
                    })
                    .cloned();
                match served {
                    Some(record) => RpcReply::value(&record),
                    None => Err(RpcError::BadParams(format!("unknown model for {provider}"))),
                }
            }
            methods::LIST_HIDDEN_MODELS => {
                let rows: Vec<serde_json::Value> = self
                    .hidden_ids
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|id| serde_json::json!({ "id": id, "label": id }))
                    .collect();
                RpcReply::value(&serde_json::Value::Array(rows))
            }
            methods::SAVE_CUSTOM_PROVIDER => {
                self.custom_saves.lock().unwrap().push(params.clone());
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REMOVE_CUSTOM_PROVIDER => {
                self.removed.lock().unwrap().push(params.clone());
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SET_PROVIDER_LOGO | methods::REMOVE_PROVIDER_LOGO => {
                self.logo_calls
                    .lock()
                    .unwrap()
                    .push((method.to_string(), params.clone()));
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SAVE_PROVIDER_KEY => {
                self.saved_keys.lock().unwrap().push(params.clone());
                if let Some(key) = params["key"].as_str() {
                    self.keys.lock().unwrap().insert(
                        params["providerId"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        key.to_string(),
                    );
                }
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REVEAL_PROVIDER_KEY => RpcReply::value(&serde_json::json!({
                "key": self
                    .keys
                    .lock()
                    .unwrap()
                    .get(params["providerId"].as_str().unwrap_or_default())
                    .cloned(),
            })),
            methods::PROBE_PROVIDER => {
                self.probes.lock().unwrap().push(params.clone());
                RpcReply::value(&serde_json::json!({
                    "ok": true,
                    "status": "ok",
                    "latencyMs": 5,
                    "modelIds": self.probe_ids.lock().unwrap().clone(),
                    "dialect": "openai-completions",
                    "error": serde_json::Value::Null,
                }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

pub(super) struct ProvidersHarness<'a> {
    pub(super) page: Entity<ProvidersPage>,
    pub(super) visual: &'a mut gpui::VisualTestContext,
    pub(super) engine: std::sync::Arc<FakeProvidersEngine>,
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) _dir: tempfile::TempDir,
}

impl ProvidersHarness<'_> {
    pub(super) fn pump(&self) {
        for _ in 0..8 {
            self.runtime
                .block_on(async { tokio::task::yield_now().await });
            self.visual.run_until_parked();
        }
    }

    pub(super) fn click(&mut self, selector: &'static str) {
        let bounds = self
            .visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} renders"));
        self.visual
            .simulate_click(bounds.center(), Default::default());
        self.pump();
    }
}

pub(super) fn providers_harness<'a>(cx: &'a mut gpui::TestAppContext) -> ProvidersHarness<'a> {
    let engine = std::sync::Arc::new(FakeProvidersEngine {
        catalog: std::sync::Mutex::new(vec![
            serde_json::json!({
                "id": "acme/acme-1",
                "provider": "acme",
                "label": "Acme 1",
            }),
            serde_json::json!({
                "id": "acme/acme-custom",
                "provider": "acme",
                "label": "Acme custom",
                "custom": true,
            }),
            serde_json::json!({
                "id": "beta/beta-1",
                "provider": "beta",
                "label": "Beta 1",
            }),
        ]),
        records: std::sync::Mutex::new(Vec::new()),
        custom_saves: std::sync::Mutex::new(Vec::new()),
        saved_keys: std::sync::Mutex::new(Vec::new()),
        probes: std::sync::Mutex::new(Vec::new()),
        probe_ids: std::sync::Mutex::new(vec!["acme-9".to_string(), "acme-1".to_string()]),
        hidden_ids: std::sync::Mutex::new(Vec::new()),
        keys: std::sync::Mutex::new(Default::default()),
        logo_calls: std::sync::Mutex::new(Vec::new()),
        removed: std::sync::Mutex::new(Vec::new()),
        preview_rows: std::sync::Mutex::new(Vec::new()),
        preview_reply: std::sync::Mutex::new(None),
        fetched_saves: std::sync::Mutex::new(Vec::new()),
        records_served: std::sync::Mutex::new(Vec::new()),
        preview_error: std::sync::Mutex::new(None),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        cx.set_global(Theme::default());
        crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
    });
    let app_state = cx.new(|_| AppState::new());
    let client = {
        let _guard = runtime.enter();
        holt_rpc::memory_client(engine.clone())
    };
    app_state.update(cx, |state, cx| state.attach_test_engine(client, cx));
    let (page, visual) =
        cx.add_window_view(|_window, cx| ProvidersPage::new(app_state.clone(), cx));
    let harness = ProvidersHarness {
        page,
        visual,
        engine,
        runtime,
        _dir: dir,
    };
    harness.pump();
    harness
}
