//! The provider/model catalog surface (ADR-0028/0031/0037): keys, probes,
//! records, logos, proposals, and the settle cards.

use holt_proto::RunOutcome;
use holt_rpc::{RpcError, RpcReply};
use pi_core::ai::auth::types::CredentialStore;
use pi_core::ai::types::Model as CoreModel;

use super::{optional_string, required_string, required_string_list};
use crate::EngineService;

impl EngineService {
    /// Re-seed every open chat's occupancy windows after a catalog write:
    /// the denominator map is seeded once per watch open, so a new or
    /// changed record would otherwise serve a stale window until the watch
    /// reopened.
    fn refresh_catalog_windows(&self) {
        for chat in self.runtime.open_chats() {
            let chat_id = chat.chat_id.clone();
            crate::usage::seed_occupancy(
                &chat,
                self.providers.context_windows().into_iter().collect(),
                self.selected_wire_model(&chat_id),
            );
            crate::usage::publish(&chat);
        }
    }

    /// The chat's selected model as the occupancy windows are keyed — the
    /// denominator `WatchChatUsage` falls back to when the queue holds
    /// nothing next. `None` for a row without a config yet (a chat that has
    /// never run has no selection to divide by).
    pub(super) fn selected_wire_model(&self, chat_id: &str) -> Option<String> {
        let chats = self.runtime.chats.read().ok()?;
        let config = chats
            .iter()
            .find(|row| row.id == chat_id)?
            .config
            .as_ref()?;
        Some(crate::usage::wire_model_id(
            &config.provider.0,
            &config.model,
        ))
    }

    /// Point an OPEN chat runtime's denominator at its current selection.
    /// Cheap and idempotent: an unopened chat has no watch to correct yet —
    /// the subscription seeds it from the same source.
    pub(super) fn refresh_selected_model(&self, chat_id: &str) {
        if let Some(chat) = self.runtime.loaded_chat(chat_id) {
            crate::usage::set_selected_model(&chat, self.selected_wire_model(chat_id));
        }
    }

    pub(super) async fn save_provider_key(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let key = required_string(&params, "key")?;
        if !self.providers.is_eligible(provider) {
            return Err(RpcError::BadParams(
                "unknown or unsupported provider".into(),
            ));
        }
        self.providers
            .credentials
            .save_key(provider, key)
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) async fn probe_provider(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        if !self.providers.is_eligible(provider) {
            return Err(RpcError::BadParams(
                "unknown or unsupported provider".into(),
            ));
        }
        let reply = match crate::tools::model_setup::probe_target(&self.providers, provider) {
            None => serde_json::json!({
                "ok": false,
                "status": "unverifiable",
                "latencyMs": serde_json::Value::Null,
                "modelIds": [],
                "dialect": serde_json::Value::Null,
                "error": "no transport to probe",
            }),
            Some((base_url, dialect)) => {
                let key = self.providers.credentials.reveal_key(provider).await;
                let started = std::time::Instant::now();
                let result = crate::tools::model_setup::probe_models(
                    &base_url,
                    &dialect,
                    key.as_deref(),
                    None,
                )
                .await;
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                match result {
                    Ok(model_ids) => serde_json::json!({
                        "ok": true,
                        "status": "ok",
                        "latencyMs": latency_ms,
                        "modelIds": model_ids,
                        "dialect": dialect,
                        "error": serde_json::Value::Null,
                    }),
                    Err(error) => {
                        // Only a 401/403 challenge is a verdict on
                        // the key: every probeable dialect's header
                        // style matches what probe_models sends, so
                        // a rejection means the stored key is wrong.
                        // Anything else — 404, 5xx, timeout, an
                        // unparseable body — verifies nothing either
                        // way, because /models may be absent or
                        // unauthenticated while the key is fine.
                        let status = if error == "HTTP 401" || error == "HTTP 403" {
                            "key_rejected"
                        } else {
                            "unverifiable"
                        };
                        serde_json::json!({
                            "ok": false,
                            "status": status,
                            "latencyMs": latency_ms,
                            "modelIds": [],
                            "dialect": dialect,
                            "error": error,
                        })
                    }
                }
            }
        };
        RpcReply::value(&reply)
    }

    pub(super) async fn reveal_provider_key(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        RpcReply::value(&serde_json::json!({
            "key": self.providers.credentials.reveal_key(provider).await
        }))
    }

    pub(super) async fn remove_provider_key(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        self.providers
            .credentials
            .delete(provider, None)
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn list_models(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        RpcReply::value(&self.providers.models_for(provider))
    }

    pub(super) fn list_hidden_models(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let rows: Vec<serde_json::Value> = self
            .providers
            .settings
            .hidden_models_for(provider)
            .iter()
            .map(|model_id| {
                let label = self
                    .providers
                    .resolve_model(provider, &format!("{provider}/{model_id}"))
                    .ok()
                    .map(|model| model.name);
                serde_json::json!({ "id": format!("{provider}/{model_id}"), "label": label })
            })
            .collect();
        RpcReply::value(&rows)
    }

    // The proposal card's Write (ADR-0037): the write path the agent
    // never holds. Revalidation rides `apply_changes`; the button is
    // the human approval. A gone proposal (superseded, evicted,
    // settled) stamps its card so no stale Write lingers; any other
    // failure leaves the card Pending with the error to show.
    pub(super) fn apply_model_proposal(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let proposal_id = required_string(&params, "proposalId")?;
        let chat = self.runtime.chat(chat_id);
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        if crate::tools::model_setup::stored_proposal(&chat, proposal_id).is_none() {
            crate::provider_mode::stamp_proposal_card(
                &chat,
                proposal_id,
                holt_doc::parts::ProposalCardState::Superseded,
            );
            return Err(RpcError::Failed(GONE_PROPOSAL.into()));
        }
        let applied = crate::tools::model_setup::apply_stored(&self.providers, &chat, proposal_id)
            .map_err(RpcError::Failed)?;
        crate::provider_mode::stamp_proposal_card(
            &chat,
            proposal_id,
            holt_doc::parts::ProposalCardState::Written,
        );
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({ "applied": applied }))
    }

    pub(super) fn discard_model_proposal(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let proposal_id = required_string(&params, "proposalId")?;
        let chat = self.runtime.chat(chat_id);
        if !crate::tools::model_setup::discard_stored(&chat, proposal_id) {
            crate::provider_mode::stamp_proposal_card(
                &chat,
                proposal_id,
                holt_doc::parts::ProposalCardState::Superseded,
            );
            return Err(RpcError::Failed(GONE_PROPOSAL.into()));
        }
        crate::provider_mode::stamp_proposal_card(
            &chat,
            proposal_id,
            holt_doc::parts::ProposalCardState::Discarded,
        );
        RpcReply::value(&serde_json::json!({ "discarded": true }))
    }

    // The settle (ADR-0031): one engine-owned operation — save the
    // key (never the chat), queue the fixed notice, clear the card.
    pub(super) async fn settle_provider_key_request(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let chat = self.runtime.chat(chat_id);
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        // Take (not clone) the request: a second concurrent settle
        // finds none, and any failure below puts it back so the
        // card stays and a retry is idempotent.
        let pending = chat
            .key_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(pending) = pending else {
            // A card left Pending past its request (a newer one
            // replaced it) must not keep offering Save.
            crate::provider_mode::stamp_key_cards(&chat, holt_doc::parts::KeyCardState::Superseded);
            return Err(RpcError::Failed(
                "no pending key request on this chat".into(),
            ));
        };
        let restore = || {
            *chat.key_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending.clone());
            chat.save_provider_mode();
        };
        let saved = if let Some(key) = params
            .get("key")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            if let Err(error) = self
                .providers
                .credentials
                .save_key(&pending.provider_id, key)
                .await
            {
                restore();
                return Err(RpcError::Failed(error.to_string()));
            }
            // The entry approves the destination the card showed
            // (ADR-0031): this chat's planned-target probes may
            // now carry the key against the exact same baseUrl.
            chat.approved_key_destinations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((pending.provider_id.clone(), pending.destination.clone()));
            chat.save_provider_mode();
            true
        } else if params.get("key").is_some() {
            restore();
            return Err(RpcError::BadParams("key must not be empty".into()));
        } else {
            false
        };
        let notice = if saved {
            crate::tools::model_setup::key_saved_notice(&pending.provider_id, &pending.destination)
        } else {
            crate::tools::model_setup::key_dismissed_notice(&pending.provider_id)
        };
        // The notice rides the queue as an ordinary user message on
        // the chat's own model — exactly what the composer
        // would have sent had the user typed it.
        let (config, cwd) = match self.chat_run_target(chat_id) {
            Ok(target) => target,
            Err(error) => {
                restore();
                return Err(error);
            }
        };
        let message_id = format!("key-request-{}", uuid::Uuid::new_v4());
        if let Err(error) = self.enqueue_run(
            chat.clone(),
            Self::queued_run_request(&config, &notice, cwd),
            message_id,
        ) {
            restore();
            return Err(error);
        }
        chat.save_provider_mode();
        crate::provider_mode::stamp_key_cards(
            &chat,
            if saved {
                holt_doc::parts::KeyCardState::Saved
            } else {
                holt_doc::parts::KeyCardState::Dismissed
            },
        );
        RpcReply::value(&serde_json::json!({
            "settled": if saved { "saved" } else { "dismissed" },
            "providerId": pending.provider_id,
            "destination": pending.destination,
        }))
    }

    pub(super) async fn settle_provider_choice(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let card_id = required_string(&params, "cardId")?;
        let provider_id = required_string(&params, "providerId")?;
        let chat = self.runtime.chat(chat_id);
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        if !self.provider_mode_state(chat_id)?.active {
            return Err(RpcError::Failed("the chat is not in Provider Mode".into()));
        }
        let (config, cwd) = self.chat_run_target(chat_id)?;
        // The stamp is the claim: a second concurrent click finds
        // the card already Chosen and is refused.
        let chosen = crate::provider_mode::choose_provider(&chat, card_id, provider_id)
            .map_err(RpcError::Failed)?;
        let notice = crate::tools::model_setup::provider_chosen_notice(&chosen.id, &chosen.name);
        let message_id = format!("provider-choice-{}", uuid::Uuid::new_v4());
        if let Err(error) = self.enqueue_run(
            chat.clone(),
            Self::queued_run_request(&config, &notice, cwd),
            message_id,
        ) {
            crate::provider_mode::unchoose_provider(&chat, card_id);
            return Err(error);
        }
        RpcReply::value(&serde_json::json!({ "providerId": chosen.id }))
    }

    pub(super) async fn settle_question(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let card_id = required_string(&params, "cardId")?;
        let answers = required_string_list(&params, "choices")?;
        let chat = self.runtime.chat(chat_id);
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        let (config, cwd) = self.chat_run_target(chat_id)?;
        // The stamp is the claim: a second concurrent answer finds
        // the card already Chosen and is refused.
        let pairs = crate::tools::ask_user::settle_question(&chat, card_id, answers)
            .map_err(RpcError::Failed)?;
        let notice = crate::tools::ask_user::question_answer_notice(&pairs);
        let message_id = format!("question-answer-{}", uuid::Uuid::new_v4());
        if let Err(error) = self.enqueue_run(
            chat.clone(),
            Self::queued_run_request(&config, &notice, cwd),
            message_id,
        ) {
            crate::tools::ask_user::unsettle_question(&chat, card_id);
            return Err(error);
        }
        let answers: Vec<String> = pairs.into_iter().map(|(_, answer)| answer).collect();
        RpcReply::value(&serde_json::json!({ "answers": answers }))
    }

    pub(super) fn dismiss_question(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let card_id = required_string(&params, "cardId")?;
        let chat = self.runtime.chat(chat_id);
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        crate::tools::ask_user::dismiss_question(&chat, card_id).map_err(RpcError::Failed)?;
        // A run waiting on the dismissed question is done.
        self.routines
            .move_run(chat_id, RunOutcome::Waiting, RunOutcome::Succeeded);
        RpcReply::value(&serde_json::json!({ "dismissed": true }))
    }

    pub(super) fn list_api_dialects(&self) -> Result<RpcReply, RpcError> {
        let ids: Vec<String> = pi_core::ai::compat::get_api_providers()
            .iter()
            .map(|provider| provider.api.clone())
            .collect();
        RpcReply::value(&serde_json::json!(ids))
    }

    pub(super) fn save_custom_provider(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "id")?.trim().to_string();
        let name = required_string(&params, "name")?.trim().to_string();
        let base_url = required_string(&params, "baseUrl")?.trim().to_string();
        let default_api = required_string(&params, "defaultApi")?.trim().to_string();
        let provider = crate::provider_settings::CustomProvider {
            id,
            name,
            base_url,
            default_api,
        };
        self.providers
            .validate_custom_provider(&provider)
            .map_err(RpcError::BadParams)?;
        self.providers
            .settings
            .upsert_custom_provider(provider)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn remove_custom_provider(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        self.providers
            .settings
            .remove_custom_provider(provider)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn set_provider_logo(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let data = required_string(&params, "data")?;
        if data.len() > crate::provider_logos::MAX_UPLOAD_BYTES.div_ceil(3) * 4 {
            return Err(RpcError::BadParams(
                "logo exceeds the 8 MiB upload limit".into(),
            ));
        }
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data.as_bytes())
                .map_err(|error| RpcError::BadParams(format!("data is not base64: {error}")))?;
        self.providers
            .settings
            .set_custom_provider_logo(provider, &bytes)
            .map_err(RpcError::BadParams)?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn remove_provider_logo(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        self.providers
            .settings
            .remove_custom_provider_logo(provider)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn save_model_record(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let mut record = params
            .get("record")
            .cloned()
            .ok_or_else(|| RpcError::BadParams("record is required".into()))?;
        // A record may omit `baseUrl`: adding a model to a provider
        // means riding the provider's endpoint, so the default fills
        // it in. An explicit value — the endpoint-fix case — wins.
        let carries_base_url = record
            .get("baseUrl")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|url| !url.trim().is_empty());
        if !carries_base_url {
            let base_url = self.providers.default_base_url(provider).ok_or_else(|| {
                RpcError::BadParams(
                    "record needs a baseUrl: the provider declares no default \
                         endpoint"
                        .into(),
                )
            })?;
            if let Some(slot) = record.as_object_mut() {
                slot.insert("baseUrl".to_string(), serde_json::Value::String(base_url));
            }
        }
        let record: CoreModel = serde_json::from_value(record)
            .map_err(|_| RpcError::BadParams("record must be a model record".into()))?;
        self.providers
            .validate_model_record(provider, &record)
            .map_err(RpcError::BadParams)?;
        self.providers
            .settings
            .upsert_model_record(provider, record)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn remove_model_record(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let submitted = required_string(&params, "modelId")?.trim();
        let qualified_prefix = format!("{provider}/");
        let model = submitted
            .strip_prefix(&qualified_prefix)
            .unwrap_or(submitted);
        let removed_record = self
            .providers
            .settings
            .remove_model_record(provider, model)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if !removed_record {
            self.providers
                .settings
                .remove_custom_model(provider, model)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn set_hidden_models(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let provider = required_string(&params, "providerId")?;
        let submitted = required_string_list(&params, "modelIds")?;
        let qualified_prefix = format!("{provider}/");
        let mut model_ids = std::collections::BTreeSet::new();
        for id in submitted.iter().map(String::as_str) {
            let id = id.trim();
            let model = id.strip_prefix(&qualified_prefix).unwrap_or(id);
            if !self.providers.has_model(provider, model) {
                return Err(RpcError::BadParams(format!(
                    "unknown model for {provider}: {model}"
                )));
            }
            model_ids.insert(model.to_string());
        }
        self.providers
            .settings
            .set_hidden_models(provider, model_ids)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn reset_provider_catalog(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        match optional_string(&params, "providerId") {
            Some(provider) => {
                self.providers
                    .settings
                    .reset_provider(&provider)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
            // Global reset: back to the compiled catalog under the
            // hand-edited overlay for every provider (ADR-0028).
            None => {
                self.providers
                    .settings
                    .reset_all()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
        }
        self.refresh_catalog_windows();
        RpcReply::value(&serde_json::json!({}))
    }
}

/// A proposal card's Write/Discard found nothing stored under its id.
const GONE_PROPOSAL: &str =
    "this proposal was replaced or already settled; ask the assistant to propose again";
