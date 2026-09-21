//! Shared test harness: the fake engine behind the dialog repros
//! (issue 03) and the visual-dialog driver.

use super::*;

// ---- The AI tab's headless repro (issue 03: no conversation renders
// after send) -----------------------------------------------

pub(super) fn entry_json(
    id: &str,
    role: &str,
    text: &str,
    created_at: i64,
    status: Option<&str>,
) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "id": id,
        "role": role,
        "parts": [{"kind": "text", "id": "p0", "text": text}],
        "createdAt": created_at,
        "deviceId": "local",
    });
    if let Some(status) = status {
        entry["status"] = serde_json::json!(status);
    }
    entry
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// The fake engine behind the dialog repro: the providers/models reads,
/// a fresh setup chat per `StartModelSetupChat` (incrementing ids,
/// `deleteChat` ops recorded), and a `WatchDocMessages` stream that
/// advances when `QueueCommand` lands — the engine's admission shape
/// (the user entry stamped at now, the assistant streaming).
pub(super) struct FakeSetupEngine {
    pub(super) doc: tokio::sync::watch::Sender<serde_json::Value>,
    pub(super) queue: tokio::sync::watch::Sender<serde_json::Value>,
    /// The picker's catalog — the second entry is the user's custom
    /// model, dropped when `ResetProviderCatalog` lands.
    pub(super) catalog: std::sync::Mutex<Vec<serde_json::Value>>,
    pub(super) resets: std::sync::Mutex<Vec<serde_json::Value>>,
    pub(super) queued: std::sync::Mutex<Vec<serde_json::Value>>,
    /// True = the NEXT turn never admits (the engine's post-QueueCommand
    /// failure shape: no doc entry, the queue frame carries the reason);
    /// consumed by the first send, so a retry succeeds.
    pub(super) fail_first_admission: std::sync::atomic::AtomicBool,
    /// The review panel's stored proposals, served verbatim by
    /// ListModelProposals (the engine's consume-on-apply included).
    pub(super) proposals: std::sync::Mutex<Vec<serde_json::Value>>,
    pub(super) applies: std::sync::Mutex<Vec<serde_json::Value>>,
    /// chatIds the page deleted (the dialog close's deleteChat op).
    pub(super) deleted_chats: std::sync::Mutex<Vec<String>>,
    /// StartModelSetupChat's fresh id sequence.
    pub(super) chat_seq: std::sync::atomic::AtomicUsize,
    /// SaveModelRecord params, in call order.
    pub(super) records: std::sync::Mutex<Vec<serde_json::Value>>,
    /// True = ApplyModelProposal fails with the staleness error instead
    /// of applying.
    pub(super) fail_applies: std::sync::atomic::AtomicBool,
    /// True = no provider is configured (the fresh-install dead end the
    /// AI tab must guide out of).
    pub(super) unconfigured: std::sync::atomic::AtomicBool,
    /// The pending Key request GetProviderKeyRequest serves.
    pub(super) key_request: std::sync::Mutex<Option<serde_json::Value>>,
    /// SettleProviderKeyRequest params, in call order.
    pub(super) settles: std::sync::Mutex<Vec<serde_json::Value>>,
    /// SaveCustomProvider params, in call order.
    pub(super) custom_saves: std::sync::Mutex<Vec<serde_json::Value>>,
    /// SaveProviderKey params, in call order.
    pub(super) saved_keys: std::sync::Mutex<Vec<serde_json::Value>>,
    /// The store RevealProviderKey reads (provider → key).
    pub(super) keys: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[async_trait::async_trait]
impl holt_rpc::RpcService for FakeSetupEngine {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
        use futures::StreamExt as _;
        use holt_rpc::{RpcError, RpcReply};
        match method {
            methods::LIST_PROVIDERS => {
                let configured = !self.unconfigured.load(std::sync::atomic::Ordering::SeqCst);
                let flag = |value: bool| serde_json::json!(value);
                RpcReply::value(&serde_json::json!([
                    {
                        "id": "acme",
                        "name": "Acme",
                        "abbreviation": "A",
                        "configured": flag(configured),
                        "variants": [{
                            "id": "acme",
                            "name": "Acme",
                            "configured": flag(configured),
                        }],
                        "custom": false,
                    },
                    {
                        "id": "beta",
                        "name": "Beta Labs",
                        "abbreviation": "B",
                        "configured": flag(configured),
                        "variants": [{
                            "id": "beta",
                            "name": "Beta Labs",
                            "configured": flag(configured),
                        }],
                        "custom": false,
                    },
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
                self.resets.lock().unwrap().push(params.clone());
                self.catalog
                    .lock()
                    .unwrap()
                    .retain(|row| row["id"] != "acme/acme-custom");
                RpcReply::value(&serde_json::json!({}))
            }
            methods::START_MODEL_SETUP_CHAT => {
                let seq = self
                    .chat_seq
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                RpcReply::value(&serde_json::json!({ "chatId": format!("setup-chat-{seq}") }))
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
            methods::MUTATE => {
                if params["op"].as_str() == Some("deleteChat") {
                    self.deleted_chats
                        .lock()
                        .unwrap()
                        .push(params["chatId"].as_str().unwrap_or_default().to_string());
                }
                RpcReply::value(&serde_json::json!({}))
            }
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
            methods::GET_PROVIDER_KEY_REQUEST => RpcReply::value(
                &self
                    .key_request
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(serde_json::json!({})),
            ),
            methods::SETTLE_PROVIDER_KEY_REQUEST => {
                self.settles.lock().unwrap().push(params.clone());
                self.key_request.lock().unwrap().take();
                RpcReply::value(&serde_json::json!({
                    "settled": if params.get("key").is_some() { "saved" } else { "dismissed" },
                    "providerId": "beta",
                    "destination": "https://api.beta.example/v1",
                }))
            }
            methods::LIST_MODEL_PROPOSALS => RpcReply::value(&serde_json::Value::Array(
                self.proposals.lock().unwrap().clone(),
            )),
            methods::APPLY_MODEL_PROPOSAL => {
                if self.fail_applies.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(RpcError::Failed(
                        "the catalog changed since this proposal (it is now a no-op); \
                         run model_proposal again"
                            .into(),
                    ));
                }
                self.applies.lock().unwrap().push(params.clone());
                let proposal_id = params["proposalId"].as_str().unwrap_or_default();
                self.proposals
                    .lock()
                    .unwrap()
                    .retain(|proposal| proposal["id"] != serde_json::json!(proposal_id));
                RpcReply::value(&serde_json::json!({ "applied": ["acme/acme-2"] }))
            }
            methods::DISCARD_MODEL_PROPOSAL => {
                let proposal_id = params["proposalId"].as_str().unwrap_or_default();
                self.proposals
                    .lock()
                    .unwrap()
                    .retain(|proposal| proposal["id"] != serde_json::json!(proposal_id));
                RpcReply::value(&serde_json::json!({ "discarded": true }))
            }
            methods::DELETE_QUEUED_MESSAGE => {
                let message_id = params["messageId"].as_str().unwrap_or_default();
                let mut entries = self.queue.subscribe().borrow_and_update().clone();
                if let Some(pending) = entries["pending"].as_array_mut() {
                    pending.retain(|item| item["messageId"] != serde_json::json!(message_id));
                }
                if entries["pending"].as_array().is_some_and(|p| p.is_empty()) {
                    entries["paused"] = serde_json::json!(false);
                }
                self.queue.send_replace(entries);
                RpcReply::value(&serde_json::json!({}))
            }
            methods::QUEUE_COMMAND => {
                self.queued.lock().unwrap().push(params.clone());
                let prompt = params["command"]["request"]["prompt"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let turn_user_id = params["command"]["messageId"]
                    .as_str()
                    .unwrap_or("setup-turn")
                    .to_string();
                if self
                    .fail_first_admission
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    // The driver's settle: the head keeps its error, the
                    // queue pauses — nothing reaches the doc.
                    self.queue.send_replace(serde_json::json!({
                        "pending": [{
                            "messageId": turn_user_id,
                            "request": params["command"]["request"],
                            "kind": "ordinary",
                            "submittedAt": 0,
                            "error": "provider deepseek is not configured",
                        }],
                        "paused": true,
                        "activeMessageId": null,
                        "error": null,
                    }));
                    return RpcReply::value(&serde_json::json!({}));
                }
                let mut entries = self
                    .doc
                    .subscribe()
                    .borrow_and_update()
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                entries.push(entry_json(&turn_user_id, "user", &prompt, now_ms(), None));
                entries.push(entry_json(
                    "setup-turn-assistant",
                    "assistant",
                    "Researching the catalog…",
                    now_ms(),
                    Some("streaming"),
                ));
                self.doc.send_replace(serde_json::json!(entries));
                // The queue state is re-published untouched: whatever
                // the sender left parked (an undeleted errored head)
                // must re-surface on the strip, or the retry test
                // cannot tell a real delete from a hidden one.
                let queue = self.queue.subscribe().borrow_and_update().clone();
                self.queue.send_replace(queue);
                RpcReply::value(&serde_json::json!({}))
            }
            methods::WATCH_MESSAGE_QUEUE => {
                let rx = self.queue.subscribe();
                let stream = futures::stream::unfold((rx, true), |(mut rx, first)| async move {
                    if !first && rx.changed().await.is_err() {
                        return None;
                    }
                    let frame = rx.borrow_and_update().clone();
                    Some((frame, (rx, false)))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::WATCH_DOC_MESSAGES => {
                let rx = self.doc.subscribe();
                let stream = futures::stream::unfold((rx, true), |(mut rx, first)| async move {
                    if !first && rx.changed().await.is_err() {
                        return None;
                    }
                    let entries = rx.borrow_and_update().clone();
                    Some((serde_json::json!({ "reset": entries }), (rx, false)))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

pub(super) struct SetupHarness<'a> {
    pub(super) page: Entity<ProvidersPage>,
    pub(super) state: Entity<AppState>,
    pub(super) visual: &'a mut gpui::VisualTestContext,
    pub(super) engine: std::sync::Arc<FakeSetupEngine>,
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) _dir: tempfile::TempDir,
}

impl SetupHarness<'_> {
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

pub(super) fn setup_dialog_harness<'a>(cx: &'a mut gpui::TestAppContext) -> SetupHarness<'a> {
    setup_dialog_harness_with(cx, false)
}

pub(super) fn setup_dialog_harness_with<'a>(
    cx: &'a mut gpui::TestAppContext,
    fail_first_admission: bool,
) -> SetupHarness<'a> {
    let (doc, _doc_rx) = tokio::sync::watch::channel(serde_json::json!([]));
    let (queue, _queue_rx) = tokio::sync::watch::channel(serde_json::json!({
        "pending": [],
        "paused": false,
        "activeMessageId": null,
        "error": null,
    }));
    let engine = std::sync::Arc::new(FakeSetupEngine {
        doc,
        queue,
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
        resets: std::sync::Mutex::new(Vec::new()),
        queued: std::sync::Mutex::new(Vec::new()),
        fail_first_admission: std::sync::atomic::AtomicBool::new(fail_first_admission),
        proposals: std::sync::Mutex::new(Vec::new()),
        applies: std::sync::Mutex::new(Vec::new()),
        fail_applies: std::sync::atomic::AtomicBool::new(false),
        unconfigured: std::sync::atomic::AtomicBool::new(false),
        deleted_chats: std::sync::Mutex::new(Vec::new()),
        chat_seq: std::sync::atomic::AtomicUsize::new(0),
        records: std::sync::Mutex::new(Vec::new()),
        key_request: std::sync::Mutex::new(None),
        settles: std::sync::Mutex::new(Vec::new()),
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
    let harness = SetupHarness {
        page,
        state: app_state,
        visual,
        engine,
        runtime,
        _dir: dir,
    };
    harness.pump();
    harness
}
