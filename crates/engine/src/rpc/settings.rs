//! The settings quartets: title, web search, Jev, MCP
//! (ADR-0012/0023/0027/0034).

use holt_doc::MessageRole;
use holt_proto::{
    JevSettingsState, TitleSettings, TitleSettingsState, TitleSource, WebSearchBackendOption,
    WebSearchEntryView, WebSearchSettingsState,
};
use holt_rpc::{RpcError, RpcReply};
use std::sync::Arc;

use super::{optional_string, required_string};
use crate::EngineService;
use crate::agent::ChatRuntime;
use crate::title_settings::MAX_TITLE_INSTRUCTION_CHARS;

impl EngineService {
    pub(super) async fn title_settings_state(&self) -> TitleSettingsState {
        let settings = self.title_settings.get();
        let warning = self.title_settings_warning(&settings).await;
        TitleSettingsState { settings, warning }
    }

    /// Missing credentials are a visible warning, never an error: the title
    /// task fails silently and normal chat Turns are unaffected.
    async fn title_settings_warning(&self, settings: &TitleSettings) -> Option<String> {
        let model_id = settings.model_id.as_deref()?;
        let provider = model_id.split('/').next()?;
        if self
            .providers
            .credentials
            .reveal_key(provider)
            .await
            .is_some()
        {
            return None;
        }
        Some(format!(
            "Provider {provider} has no saved credentials — automatic titles will keep the fallback until a key is configured."
        ))
    }

    /// The web-search settings view (ADR-0023) — the reply shape of every
    /// web-search RPC but reveal. Raw keys never ride this view. Entries
    /// whose kind is no longer offered (a custom definition since removed
    /// or broken) stay on disk, key included, but are left out here — and
    /// an active one shows as off, which is what it mounts.
    pub(super) fn web_search_state(&self) -> WebSearchSettingsState {
        let (custom, custom_error) = match self.custom_search_backends() {
            Ok(custom) => (custom, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let backends = web_search_options(&custom);
        let settings = self.web_search.get();
        let entries = settings
            .entries
            .iter()
            .filter_map(|entry| {
                let backend = backends.iter().find(|backend| backend.id == entry.kind)?;
                Some(WebSearchEntryView {
                    id: entry.id.clone(),
                    kind: entry.kind.clone(),
                    name: backend.name.clone(),
                    api_key_masked: (!entry.api_key.is_empty()).then(|| masked_key(&entry.api_key)),
                })
            })
            .collect::<Vec<_>>();
        WebSearchSettingsState {
            active: settings
                .active
                .filter(|id| entries.iter().any(|entry| entry.id == *id)),
            entries,
            backends,
            custom_file: self
                .data_dir
                .join(crate::tools::web_search::custom::FILE_NAME)
                .display()
                .to_string(),
            custom_error,
        }
    }

    /// The user's `search-backends.json` definitions, read fresh.
    fn custom_search_backends(
        &self,
    ) -> Result<Vec<crate::tools::web_search::custom::CustomBackend>, String> {
        crate::tools::web_search::custom::load(&self.data_dir)
    }

    /// The Jev settings view (ADR-0027) — the reply shape of the read and
    /// save RPCs. The raw key never rides this view.
    pub(super) fn jev_state(&self) -> JevSettingsState {
        JevSettingsState {
            api_key_masked: self.jev.get().map(|record| masked_key(&record.api_key)),
        }
    }

    pub(super) async fn save_jev_settings(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let key = required_string(&params, "apiKey")?;
        self.jev
            .save(key)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.jev_state())
    }

    /// The MCP settings view (ADR-0034): every definition — flat,
    /// camelCase, exactly the hand-editable `mcpServers` entry shape —
    /// plus any file-level validation error from a hand edit that landed
    /// since startup (a broken file keeps the last-good set).
    pub(super) async fn mcp_settings_state(&self) -> serde_json::Value {
        let validation_error = self.runtime.mcp.refresh_error();
        let servers: Vec<serde_json::Value> = self
            .runtime
            .mcp
            .definitions()
            .into_iter()
            .map(|(name, server)| {
                let mut value = crate::mcp::config::server_to_value(&server);
                let object = value.as_object_mut().unwrap();
                object.insert("name".into(), serde_json::json!(name));
                // The UI view carries the shared fields explicitly, even
                // when the file form omits defaults.
                object.insert("enabled".into(), serde_json::json!(server.enabled));
                object.insert(
                    "startupTimeoutMs".into(),
                    serde_json::json!(server.startup_timeout_ms),
                );
                object.insert(
                    "toolTimeoutMs".into(),
                    serde_json::json!(server.tool_timeout_ms),
                );
                object.insert(
                    "enabledTools".into(),
                    serde_json::json!(server.enabled_tools),
                );
                object.insert(
                    "disabledTools".into(),
                    serde_json::json!(server.disabled_tools),
                );
                value
            })
            .collect();
        serde_json::json!({
            "servers": servers,
            "validationError": validation_error,
        })
    }

    /// `SaveMcpServer` — the strict upsert. The name must satisfy
    /// `[A-Za-z0-9_-]` (the two-level tool naming never becomes
    /// ambiguous) and the definition survives the same strict parse a
    /// hand-edited file would; the write is atomic under the credentials
    /// pattern, and the server's cached connection is dropped so the
    /// next Turn serves the new definition.
    pub(super) async fn save_mcp_server(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let name = required_string(&params, "name")?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(RpcError::BadParams(format!(
                "invalid mcp server name {name:?}: names use [A-Za-z0-9_-] only"
            )));
        }
        let definition = params
            .get("server")
            .cloned()
            .ok_or_else(|| RpcError::BadParams("server is required".into()))?;
        let server =
            crate::mcp::config::parse_server(name, &definition).map_err(RpcError::BadParams)?;
        let mut servers = self.runtime.mcp.definitions();
        servers.insert(name.to_string(), server);
        self.runtime
            .mcp
            .store()
            .save(servers)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.mcp.invalidate(name).await;
        RpcReply::value(&self.mcp_settings_state().await)
    }

    /// `RemoveMcpServer` — delete one definition (its cached connection
    /// dies with it).
    pub(super) async fn remove_mcp_server(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let name = required_string(&params, "name")?;
        let mut servers = self.runtime.mcp.definitions();
        if servers.remove(name).is_none() {
            return Err(RpcError::BadParams(format!("unknown mcp server {name:?}")));
        }
        self.runtime
            .mcp
            .store()
            .save(servers)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.mcp.invalidate(name).await;
        RpcReply::value(&self.mcp_settings_state().await)
    }

    /// `TestMcpServer` — the probe reply: status, tool count and names,
    /// or the failure reason. Reads the current definitions without
    /// disturbing the pool's live connections.
    pub(super) async fn mcp_probe_reply(&self, name: &str) -> serde_json::Value {
        match self.runtime.mcp.probe(name).await {
            crate::mcp::ProbeReport::Ok {
                tool_count,
                tool_names,
            } => serde_json::json!({
                "status": "ok",
                "toolCount": tool_count,
                "toolNames": tool_names,
            }),
            crate::mcp::ProbeReport::Failed { reason } => {
                serde_json::json!({ "status": "failed", "reason": reason })
            }
        }
    }

    /// Validate and store one backend entry, making it active. The kind is
    /// a built-in or a current custom definition; a keyed backend requires
    /// `apiKey`, a keyless one ignores it.
    pub(super) fn save_web_search_backend(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let kind = required_string(&params, "kind")?;
        let known = web_search_options(&self.custom_search_backends().unwrap_or_default());
        let Some(backend) = known.iter().find(|backend| backend.id == kind) else {
            return Err(RpcError::BadParams(format!(
                "unknown search backend {kind:?}; expected one of {}",
                known
                    .iter()
                    .map(|backend| backend.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        };
        let api_key = if backend.needs_key {
            let api_key = optional_string(&params, "apiKey").unwrap_or_default();
            if api_key.trim().is_empty() {
                return Err(RpcError::BadParams("apiKey is required".into()));
            }
            api_key
        } else {
            String::new()
        };
        self.web_search
            .save(crate::web_search_settings::WebSearchEntry::new(
                kind, api_key,
            ))
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.web_search_state())
    }

    /// The Turn's web-search backend (ADR-0023), resolved once per Turn
    /// admission — a mid-Turn settings change lands from the next Turn,
    /// like the permission mode. No active entry — or one whose adapter
    /// cannot mount — resolves to no backend, so `web_search` stays out of
    /// the toolset. The injected test resolver sees the entry's kind.
    pub(super) fn search_backend(&self) -> Option<Arc<dyn crate::SearchBackend>> {
        let settings = self.web_search.get();
        let entry = settings.active_entry()?;
        match &self.search_backend_resolver {
            Some(resolve) => resolve(&entry.kind),
            None => {
                let custom = self.custom_search_backends().unwrap_or_else(|error| {
                    tracing::warn!(%error, "custom search backends left out");
                    Vec::new()
                });
                crate::tools::web_search::adapter(entry, &custom)
            }
        }
    }

    /// Cheap read-side pre-check for the Title task's eligibility window:
    /// an untitled, automatically-owned chat whose one-shot task has not
    /// started. Only a first prompt can satisfy this — the fallback title
    /// is stamped in the same acceptance pass.
    pub(super) fn title_may_be_eligible(&self, chat_id: &str, chat: &ChatRuntime) -> bool {
        let has_user_prompt = chat
            .transcript
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|entry| entry.role == MessageRole::User)
            || chat
                .history
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|message| message.role() == "user");
        if has_user_prompt {
            return false;
        }
        let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
        chats.iter().any(|row| {
            row.id == chat_id
                && row.title.is_none()
                && row.title_source == TitleSource::Automatic
                && !row.title_task_started
        })
    }

    /// Resolve the Title task's inputs from the current settings. `None`
    /// means no task: automatic titles disabled, an unresolvable model, or
    /// missing credentials — all silent, the fallback title stays.
    pub(super) async fn prepare_title_task(
        &self,
        chat_id: &str,
        chat: &Arc<crate::agent::ChatRuntime>,
        prompt: &str,
    ) -> Option<crate::title_task::TitleTaskSpec> {
        let settings = self.title_settings.get();
        let model_id = settings.model_id?;
        let (provider, _) = model_id.split_once('/')?;
        if !self.providers.is_eligible(provider) {
            return None;
        }
        let model = self.providers.resolve_model(provider, &model_id).ok()?;
        let api_key = self.providers.credentials.reveal_key(provider).await?;
        Some(crate::title_task::TitleTaskSpec {
            chat_id: chat_id.to_string(),
            data_dir: self.data_dir.clone(),
            // The chat whose ledger bills the round-trip; the run's own
            // runtime handle, so the title record lands on the live totals.
            chat: chat.clone(),
            prompt: prompt.to_string(),
            instruction: settings.instruction,
            model,
            api_key,
            stream_fn: self.runtime.stream_fn.clone(),
        })
    }

    pub(super) async fn save_title_settings(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let mut settings: TitleSettings = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        // An empty/whitespace model id is the disabled state, not an error.
        settings.model_id = settings
            .model_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        if let Some(model_id) = settings.model_id.as_deref() {
            let Some((provider, _)) = model_id.split_once('/') else {
                return Err(RpcError::BadParams(format!(
                    "model must use provider/model syntax: {model_id}"
                )));
            };
            if !self.providers.is_eligible(provider) {
                return Err(RpcError::BadParams(format!(
                    "unknown or unsupported provider: {provider}"
                )));
            }
            self.providers
                .resolve_model(provider, model_id)
                .map_err(RpcError::BadParams)?;
        }
        let instruction = settings.instruction.trim();
        if instruction.is_empty() {
            return Err(RpcError::BadParams("instruction must not be empty".into()));
        }
        if instruction.chars().count() > MAX_TITLE_INSTRUCTION_CHARS {
            return Err(RpcError::BadParams(format!(
                "instruction must be at most {MAX_TITLE_INSTRUCTION_CHARS} characters"
            )));
        }
        settings.instruction = instruction.to_string();
        self.title_settings
            .save(settings)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.title_settings_state().await)
    }

    // A null (or absent) id turns web search off; saved entries stay.
    pub(super) fn set_active_web_search_backend(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = optional_string(&params, "id");
        if let Some(id) = id.as_deref()
            && self.web_search.get().entry(id).is_none()
        {
            return Err(RpcError::BadParams(format!(
                "no search backend with id {id:?}"
            )));
        }
        self.web_search
            .set_active(id.as_deref())
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.web_search_state())
    }

    pub(super) fn reveal_web_search_key(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "id")?;
        RpcReply::value(&serde_json::json!({
            "key": self
                .web_search
                .get()
                .entry(id)
                .map(|entry| entry.api_key.clone())
                .filter(|key| !key.is_empty()),
        }))
    }

    pub(super) fn remove_web_search_backend(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        self.web_search
            .remove(required_string(&params, "id")?)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.web_search_state())
    }

    pub(super) fn remove_jev_settings(&self) -> Result<RpcReply, RpcError> {
        self.jev
            .remove()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }
}

/// The Settings picker's options (ADR-0023): the built-ins in picker
/// order, then the user's definitions in file order.
fn web_search_options(
    custom: &[crate::tools::web_search::custom::CustomBackend],
) -> Vec<WebSearchBackendOption> {
    crate::tools::web_search::BACKENDS
        .iter()
        .map(|backend| WebSearchBackendOption {
            id: backend.id.to_string(),
            name: backend.name.to_string(),
            needs_key: backend.needs_key,
        })
        .chain(custom.iter().map(|backend| WebSearchBackendOption {
            id: backend.id.clone(),
            name: backend.name.clone(),
            needs_key: backend.needs_key,
        }))
        .collect()
}

/// Mask a stored settings key (search or Jev) for display: the first and
/// last four characters joined by an ellipsis. At least one character must
/// stay hidden, so keys of eight or fewer characters reveal nothing at all.
fn masked_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}
