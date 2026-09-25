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
    /// The store RevealProviderKey reads (provider → key).
    pub(super) keys: std::sync::Mutex<std::collections::HashMap<String, String>>,
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
            methods::LIST_HIDDEN_MODELS => RpcReply::value(&serde_json::Value::Array(Vec::new())),
            methods::SAVE_CUSTOM_PROVIDER => {
                self.custom_saves.lock().unwrap().push(params.clone());
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
        keys: std::sync::Mutex::new(Default::default()),
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
