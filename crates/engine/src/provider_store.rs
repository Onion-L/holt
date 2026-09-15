//! The provider store: `<data_dir>/provider-store.json`, the catalog Holt
//! owns. Boot writes the compiled `pi-core-rs` catalog there when the file
//! is missing and otherwise reads it, overlaying it on the compiled
//! baseline: providers match by id (a non-built-in id is dropped), models
//! match by id within their provider (a file entry replaces the compiled
//! record outright — the file stores complete records, so per-field merging
//! is not expressible — and file-only ids are appended), and a provider
//! entry overrides only `baseUrl` and `headers`; `name`, `organizationId`,
//! and `auth` always stay with the compiled catalog. The merged [`Catalog`]
//! is what provider listing, model listing, model resolution, context
//! windows, and eligibility answer from, so an override reaches the request
//! path; it is built once per boot, so a file edit needs a restart.
//!
//! The file is user-owned and hand-editable, so it is never rewritten once
//! it exists, and validation is per entry: an entry that fails a check (or
//! deserialization) is dropped with a log line while the rest of the file
//! still applies, and an unparsable file, an unsupported `version`, or an
//! unreadable path falls back to the compiled catalog — none of that fails
//! boot.
//!
//! The baseline source is `builtin_providers()` plus each provider's own
//! `get_models()` — never `ProviderAdapter`, whose `Models` instance
//! carries injected credentials and whose projection adds Holt-owned custom
//! model ids. Holt's availability policy (`eligible_providers()` in
//! `providers.rs`) is deliberately not applied here: the file is provider
//! data, filtered at read time.
//!
//! On Unix the file carries mode `0600` like `provider-credentials.json`:
//! it holds no secret, but it decides which host the API key is sent to,
//! so the write path pins the mode and a broader existing mode is tightened
//! on load (a failed tightening is logged, not fatal).

use std::{collections::HashMap, io::Write, path::Path};

use pi_core::ai::{
    compat,
    providers::builtin::{builtin_providers, get_builtin_model_data_generated_at},
    types::{Model as CoreModel, ModelCost, ModelCostRates, ProviderHeaders},
};
use serde::{Deserialize, Serialize};

use crate::EngineError;

const FILE_NAME: &str = "provider-store.json";

/// Envelope version. A later shape change bumps it so readers can detect the
/// difference instead of guessing.
const VERSION: u32 = 1;

/// The merged catalog: the compiled baseline with the file's valid entries
/// overlaid. Built once at boot; everything provider-shaped in the engine
/// reads it.
pub(crate) struct Catalog {
    providers: Vec<CatalogProvider>,
}

impl Catalog {
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, CatalogProvider> {
        self.providers.iter()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&CatalogProvider> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    /// The provider's merged model list; empty for an unknown id.
    pub(crate) fn models(&self, provider_id: &str) -> &[CoreModel] {
        self.get(provider_id)
            .map(|provider| provider.models.as_slice())
            .unwrap_or(&[])
    }
}

/// One catalog row. `name`, `organization_id`, and `auth` always come from
/// the compiled catalog (renaming and regrouping are UI concerns, and the
/// auth shape is the crate's contract); the file may override only
/// `base_url`, `headers`, and the model entries.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    /// `null` when the provider stands alone.
    pub(crate) organization_id: Option<String>,
    /// `null` when the provider declares none.
    pub(crate) base_url: Option<String>,
    /// Static provider configuration, not credential material.
    pub(crate) headers: Option<ProviderHeaders>,
    /// The shape of the requirement, never the material.
    pub(crate) auth: AuthShape,
    /// Compiled order with file replacements in place and file-only
    /// additions appended in file order — nothing is sorted or
    /// deduplicated here; that is the picker projection's job.
    pub(crate) models: Vec<CoreModel>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthShape {
    pub(crate) api_key: bool,
    pub(crate) oauth: bool,
}

/// Reads (creating first when missing) the provider store and overlays it on
/// the compiled catalog. Never fails: every fallback is the compiled catalog
/// plus a log line.
pub(crate) fn load(data_dir: &Path) -> Catalog {
    let path = data_dir.join(FILE_NAME);
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => ensure_private_permissions(&path, &metadata),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) = write(data_dir) {
                tracing::warn!(
                    target: "holt::engine",
                    %error,
                    "could not write the missing provider store; using the compiled catalog"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "holt::engine",
                %error,
                "could not stat the provider store; using the compiled catalog"
            );
        }
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                target: "holt::engine",
                %error,
                "could not read the provider store; using the compiled catalog"
            );
            return compiled_catalog();
        }
    };
    let envelope: FileEnvelope = match serde_json::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(
                target: "holt::engine",
                %error,
                "provider store is unparsable; ignoring it and using the compiled catalog"
            );
            return compiled_catalog();
        }
    };
    if envelope.version != VERSION {
        tracing::warn!(
            target: "holt::engine",
            version = envelope.version,
            "provider store version is not supported; ignoring it and using the compiled catalog"
        );
        return compiled_catalog();
    }
    merge(envelope.providers)
}

/// Serializes the current built-in catalog and replaces
/// `<data_dir>/provider-store.json` atomically: a temp file in the data dir,
/// then `std::fs::rename` (which replaces an existing destination on Unix and
/// Windows alike). Only `load` calls this, and only when the file is
/// missing — an existing file is the user's and is never rewritten.
pub(crate) fn write(data_dir: &Path) -> Result<(), EngineError> {
    let bytes = serde_json::to_vec_pretty(&snapshot())
        .map_err(|error| EngineError::Other(error.to_string()))?;
    let path = data_dir.join(FILE_NAME);
    let temp = data_dir.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(EngineError::Io)
}

/// On Unix the store carries the credential file's mode: rename preserves
/// the source's mode, and the explicit call makes the destination
/// independent of that.
#[cfg(unix)]
fn ensure_private_permissions(path: &Path, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 == 0 {
        return;
    }
    let result = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    if let Err(error) = result {
        tracing::warn!(
            target: "holt::engine",
            %error,
            "could not tighten the provider store's permissions to 0600"
        );
    }
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path, _metadata: &std::fs::Metadata) {}

fn snapshot() -> ProviderStore {
    ProviderStore {
        version: VERSION,
        source_generated_at: get_builtin_model_data_generated_at(),
        written_at: chrono::Utc::now().timestamp_millis(),
        providers: compiled_catalog().providers,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderStore {
    version: u32,
    /// The compiled catalog's own generation stamp, or `null` when
    /// `pi-core-rs` reports none.
    source_generated_at: Option<i64>,
    written_at: i64,
    providers: Vec<CatalogProvider>,
}

/// The compiled baseline: one entry per `builtin_providers()` element with
/// the fields the `Provider` trait exposes and `get_models()` nested
/// verbatim.
fn compiled_catalog() -> Catalog {
    Catalog {
        providers: builtin_providers()
            .into_iter()
            .map(|provider| CatalogProvider {
                id: provider.id().to_string(),
                name: provider.name().to_string(),
                organization_id: provider.organization_id().map(str::to_string),
                base_url: provider.base_url().map(str::to_string),
                headers: provider.headers().cloned(),
                auth: AuthShape {
                    api_key: provider.auth().api_key.is_some(),
                    oauth: provider.auth().oauth.is_some(),
                },
                models: provider.get_models(),
            })
            .collect(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileEnvelope {
    version: u32,
    #[serde(default)]
    providers: Vec<serde_json::Value>,
}

/// One file entry. `name`/`organizationId`/`auth` are carried by the schema
/// but deliberately not read: identity fields always come from the compiled
/// catalog.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileProvider {
    id: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    headers: Option<ProviderHeaders>,
    #[serde(default)]
    models: Vec<serde_json::Value>,
}

/// Applies the file's provider entries to the compiled baseline. An entry
/// that fails to deserialize, carries a non-built-in id, or fails its own
/// `baseUrl` check is dropped whole; a model entry that fails any check is
/// dropped within its provider. Duplicate ids in the file resolve last-wins,
/// like a map.
fn merge(entries: Vec<serde_json::Value>) -> Catalog {
    let mut file_providers: HashMap<String, FileProvider> = HashMap::new();
    for entry in entries {
        match serde_json::from_value::<FileProvider>(entry) {
            Ok(provider) => {
                file_providers.insert(provider.id.clone(), provider);
            }
            Err(error) => {
                tracing::warn!(
                    target: "holt::engine",
                    %error,
                    "dropping a provider-store entry that does not deserialize"
                );
            }
        }
    }
    let mut catalog = compiled_catalog();
    for provider in &mut catalog.providers {
        let Some(file) = file_providers.remove(&provider.id) else {
            continue;
        };
        if file
            .base_url
            .as_deref()
            .is_some_and(|url| !http_base_url(url))
        {
            tracing::warn!(
                target: "holt::engine",
                provider = %provider.id,
                "provider-store entry has a baseUrl that is not http(s); dropping the entry"
            );
            continue;
        }
        provider.base_url = file.base_url;
        provider.headers = file.headers;
        provider.models = merge_models(&provider.id, &provider.models, file.models);
    }
    // Whatever the loop never consumed is an id the compiled catalog does
    // not carry: dropped, and the eligibility gates stay as they are.
    for id in file_providers.keys() {
        tracing::warn!(
            target: "holt::engine",
            provider = %id,
            "provider-store entry is not a built-in provider id; dropping the entry"
        );
    }
    catalog
}

/// Replaces compiled models with same-id file entries and appends file-only
/// ids in file order. A file model is dropped when it fails to deserialize
/// or breaks any validation rule; the compiled entry it would have replaced
/// then stays.
fn merge_models(
    provider_id: &str,
    compiled: &[CoreModel],
    file_models: Vec<serde_json::Value>,
) -> Vec<CoreModel> {
    let mut valid: HashMap<String, CoreModel> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for value in file_models {
        let model = match serde_json::from_value::<CoreModel>(value) {
            Ok(model) => model,
            Err(error) => {
                tracing::warn!(
                    target: "holt::engine",
                    provider = provider_id,
                    %error,
                    "dropping a provider-store model that does not deserialize"
                );
                continue;
            }
        };
        let drop = |reason: &str| {
            tracing::warn!(
                target: "holt::engine",
                provider = provider_id,
                model = %model.id,
                "dropping a provider-store model: {reason}"
            );
        };
        if model.provider != provider_id {
            drop("its provider field does not match its parent");
            continue;
        }
        if model.context_window == 0 {
            drop("its context window is zero");
            continue;
        }
        if !cost_is_valid(&model.cost) {
            drop("its cost rates are not finite and non-negative");
            continue;
        }
        if !http_base_url(&model.base_url) {
            drop("its baseUrl is not http(s)");
            continue;
        }
        if compat::get_api_provider(&model.api).is_none() {
            drop("its api dialect is not registered");
            continue;
        }
        let id = model.id.clone();
        if valid.insert(id.clone(), model).is_none() {
            order.push(id);
        }
    }
    let mut merged: Vec<CoreModel> = Vec::with_capacity(compiled.len());
    for model in compiled {
        merged.push(valid.remove(&model.id).unwrap_or_else(|| model.clone()));
    }
    for id in &order {
        if let Some(model) = valid.remove(id) {
            merged.push(model);
        }
    }
    merged
}

/// `baseUrl` is concatenated into request URLs, so it must be a usable
/// http(s) prefix, not just non-empty.
fn http_base_url(url: &str) -> bool {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .is_some_and(|rest| !rest.is_empty())
}

/// Cost rates are what the usage ledger bills, so every rate — flat and per
/// tier — must be finite and non-negative.
fn cost_is_valid(cost: &ModelCost) -> bool {
    let rates_are_valid = |rates: &ModelCostRates| {
        [
            rates.input.0,
            rates.output.0,
            rates.cache_read.0,
            rates.cache_write.0,
        ]
        .into_iter()
        .all(|rate| rate.is_finite() && rate >= 0.0)
    };
    rates_are_valid(&cost.rates)
        && cost
            .tiers
            .iter()
            .flatten()
            .all(|tier| rates_are_valid(&tier.rates))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn document(data_dir: &Path) -> serde_json::Value {
        write(data_dir).unwrap();
        serde_json::from_slice(&std::fs::read(data_dir.join(FILE_NAME)).unwrap()).unwrap()
    }

    fn optional(value: Option<&str>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, serde_json::Value::from)
    }

    /// Serves a hand-edited store: `providers` becomes the file's provider
    /// array, at version 1.
    fn serve(data_dir: &Path, providers: serde_json::Value) {
        let file = serde_json::json!({
            "version": 1,
            "sourceGeneratedAt": serde_json::Value::Null,
            "writtenAt": 1_757_928_000_000i64,
            "providers": providers,
        });
        std::fs::write(
            data_dir.join(FILE_NAME),
            serde_json::to_vec_pretty(&file).unwrap(),
        )
        .unwrap();
    }

    /// The snapshot entry of one built-in provider — the fixture every merge
    /// test mutates, so records stay complete and in sync with the real
    /// catalog without hand-writing a full `Model`.
    fn snapshot_entry(data_dir: &Path, id: &str) -> serde_json::Value {
        let value = document(data_dir);
        value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == id)
            .unwrap()
            .clone()
    }

    /// A complete `Model` record derived from a compiled one, re-id'd.
    fn reidentified(source: &serde_json::Value, id: &str) -> serde_json::Value {
        let mut model = source.clone();
        model["id"] = serde_json::Value::from(id);
        model["name"] = serde_json::Value::from(id);
        model
    }

    fn compiled_openai() -> Arc<dyn pi_core::ai::models::Provider> {
        builtin_providers()
            .into_iter()
            .find(|provider| provider.id() == "openai")
            .unwrap()
    }

    #[test]
    fn envelope_carries_version_and_millisecond_stamps() {
        let before = chrono::Utc::now().timestamp_millis();
        let store = snapshot();
        let after = chrono::Utc::now().timestamp_millis();

        assert_eq!(store.version, VERSION);
        assert_eq!(
            store.source_generated_at,
            get_builtin_model_data_generated_at()
        );
        assert!(store.written_at >= before && store.written_at <= after);
    }

    #[test]
    fn every_builtin_provider_is_written_in_catalog_order() {
        let dir = tempfile::tempdir().unwrap();
        let value = document(dir.path());
        let written: Vec<&str> = value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|provider| provider["id"].as_str().unwrap())
            .collect();
        let expected: Vec<String> = builtin_providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect();
        assert_eq!(written, expected);

        // The snapshot is provider data, not Holt's availability policy: the
        // providers `eligible_providers()` excludes are all present.
        for id in [
            "amazon-bedrock",
            "azure-openai-responses",
            "cloudflare-ai-gateway",
            "google-vertex",
            "radius",
        ] {
            assert!(written.contains(&id), "missing provider {id}");
        }
    }

    #[test]
    fn entries_mirror_the_provider_trait() {
        let dir = tempfile::tempdir().unwrap();
        let value = document(dir.path());
        for (entry, provider) in value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .zip(builtin_providers())
        {
            assert_eq!(entry["name"], provider.name());
            assert_eq!(
                entry["organizationId"],
                optional(provider.organization_id())
            );
            assert_eq!(entry["baseUrl"], optional(provider.base_url()));
            assert_eq!(
                entry["headers"],
                serde_json::to_value(provider.headers()).unwrap()
            );
            assert_eq!(
                entry["auth"]["apiKey"].as_bool(),
                Some(provider.auth().api_key.is_some())
            );
            assert_eq!(
                entry["auth"]["oauth"].as_bool(),
                Some(provider.auth().oauth.is_some())
            );
        }
    }

    #[test]
    fn models_round_trip_through_serde_in_catalog_order() {
        let dir = tempfile::tempdir().unwrap();
        let value = document(dir.path());
        for (entry, provider) in value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .zip(builtin_providers())
        {
            let source = provider.get_models();
            let models = entry["models"].as_array().unwrap();
            assert_eq!(models.len(), source.len());
            let round_tripped: Vec<CoreModel> =
                serde_json::from_value(entry["models"].clone()).unwrap();
            assert_eq!(round_tripped, source);
            for model in models {
                assert!(model["baseUrl"].is_string());
                assert!(model["api"].is_string());
            }
        }
    }

    #[test]
    fn auth_shape_is_a_boolean_requirement_not_material() {
        let dir = tempfile::tempdir().unwrap();
        // A key stored for the same data dir must not reach the file: the
        // snapshot never consults the credential store.
        std::fs::write(
            dir.path().join("provider-credentials.json"),
            br#"{"openai":"sk-sentinel-credential-value"}"#,
        )
        .unwrap();
        let value = document(dir.path());
        assert!(
            !String::from_utf8(std::fs::read(dir.path().join(FILE_NAME)).unwrap())
                .unwrap()
                .contains("sk-sentinel-credential-value")
        );

        for provider in value["providers"].as_array().unwrap() {
            for flags in ["apiKey", "oauth"] {
                assert!(
                    provider["auth"][flags].is_boolean(),
                    "{} auth.{flags} is not a boolean",
                    provider["id"]
                );
            }
        }
    }

    #[test]
    fn writes_replace_existing_content_including_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let hand_edited = br#"{"version":1,"providers":[]}"#;
        std::fs::write(&path, hand_edited).unwrap();
        assert_eq!(document(dir.path())["version"], VERSION);

        std::fs::write(&path, b"{broken").unwrap();
        let value = document(dir.path());
        assert_eq!(value["version"], VERSION);
        assert!(!value["providers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn failed_write_reports_an_error_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory at the destination makes the rename fail
        // while the rest of the data dir stays writable.
        let path = dir.path().join(FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        assert!(write(dir.path()).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_missing_file_is_created_and_the_compiled_catalog_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = load(dir.path());
        assert!(dir.path().join(FILE_NAME).is_file());

        let served: Vec<&str> = catalog
            .iter()
            .map(|provider| provider.id.as_str())
            .collect();
        let expected: Vec<String> = builtin_providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect();
        assert_eq!(
            served,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            catalog.get("openai").unwrap().models,
            compiled_openai().get_models()
        );
    }

    #[test]
    fn an_existing_file_is_read_never_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = snapshot_entry(dir.path(), "openai");
        entry["baseUrl"] = "https://proxy.example/v1".into();
        serve(dir.path(), serde_json::json!([entry]));
        let before = std::fs::read(dir.path().join(FILE_NAME)).unwrap();

        let catalog = load(dir.path());
        assert_eq!(
            catalog.get("openai").unwrap().base_url.as_deref(),
            Some("https://proxy.example/v1")
        );
        assert_eq!(std::fs::read(dir.path().join(FILE_NAME)).unwrap(), before);
    }

    #[test]
    fn a_provider_override_swaps_transport_fields_but_identity_stays_compiled() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = snapshot_entry(dir.path(), "openai");
        entry["baseUrl"] = "https://proxy.example/v1".into();
        entry["headers"] = serde_json::json!({ "x-proxy-token": "static-value" });
        entry["name"] = "Renamed".into();
        entry["organizationId"] = "elsewhere".into();
        entry["auth"] = serde_json::json!({ "apiKey": false, "oauth": true });
        serve(dir.path(), serde_json::json!([entry]));

        let catalog = load(dir.path());
        let openai = catalog.get("openai").unwrap();
        let compiled = compiled_openai();
        assert_eq!(openai.base_url.as_deref(), Some("https://proxy.example/v1"));
        assert_eq!(
            openai.headers,
            Some(ProviderHeaders::from([(
                "x-proxy-token".to_string(),
                Some("static-value".to_string())
            )]))
        );
        assert_eq!(openai.name, compiled.name());
        assert_eq!(
            openai.organization_id.as_deref(),
            compiled.organization_id()
        );
        assert_eq!(openai.auth.api_key, compiled.auth().api_key.is_some());
        assert_eq!(openai.auth.oauth, compiled.auth().oauth.is_some());
    }

    #[test]
    fn null_file_fields_override_the_compiled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = snapshot_entry(dir.path(), "openai");
        // The entry replaces the compiled record outright: the file's nulls
        // are values, not "keep the compiled field".
        entry["baseUrl"] = serde_json::Value::Null;
        entry["headers"] = serde_json::Value::Null;
        serve(dir.path(), serde_json::json!([entry]));

        let catalog = load(dir.path());
        let openai = catalog.get("openai").unwrap();
        assert_eq!(openai.base_url, None);
        assert_eq!(openai.headers, None);
    }

    #[test]
    fn a_model_entry_replaces_in_place_and_file_only_ids_are_appended() {
        let dir = tempfile::tempdir().unwrap();
        let entry = snapshot_entry(dir.path(), "openai");
        let compiled_models = entry["models"].as_array().unwrap().clone();
        let first_id = compiled_models[0]["id"].as_str().unwrap().to_string();
        let mut replacement = compiled_models[0].clone();
        replacement["contextWindow"] = 123_456.into();
        let mut fresh = reidentified(&compiled_models[0], "gpt-fresh");
        // Fields the compiled sibling does not carry, so inheritance from a
        // template would be visible.
        fresh["contextWindow"] = 7_777.into();
        fresh["reasoning"] = true.into();
        let mut edited = entry.clone();
        // File order: the file-only id first, the replacement second — the
        // append order must follow the file, not this array.
        edited["models"] = serde_json::json!([fresh, replacement]);
        serve(dir.path(), serde_json::json!([edited]));

        let catalog = load(dir.path());
        let models = &catalog.get("openai").unwrap().models;
        let position = models
            .iter()
            .position(|model| model.id == first_id)
            .unwrap();
        assert_eq!(models[position].context_window, 123_456);
        assert_eq!(models.len(), compiled_models.len() + 1);
        let appended = models.last().unwrap();
        assert_eq!(appended.id, "gpt-fresh");
        assert_eq!(appended.context_window, 7_777);
        assert!(appended.reasoning);
    }

    #[test]
    fn a_non_builtin_provider_id_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = snapshot_entry(dir.path(), "openai");
        entry["id"] = "acme-gateway".into();
        serve(dir.path(), serde_json::json!([entry]));

        let catalog = load(dir.path());
        assert!(catalog.get("acme-gateway").is_none());
        // The real provider is untouched: no override leaks across ids.
        assert_eq!(
            catalog.get("openai").unwrap().models,
            compiled_openai().get_models()
        );
    }

    #[test]
    fn an_unparsable_file_a_wrong_version_and_an_unreadable_path_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{broken").unwrap();
        let catalog = load(dir.path());
        assert_eq!(
            catalog.get("openai").unwrap().models,
            compiled_openai().get_models()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");

        serve(dir.path(), serde_json::json!([]));
        let mut file =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        file["version"] = 2.into();
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let catalog = load(dir.path());
        assert_eq!(
            catalog.get("openai").unwrap().models,
            compiled_openai().get_models()
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["version"],
            2
        );

        // A directory at the path is unreadable as a store, not a boot gate.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        let catalog = load(dir.path());
        assert_eq!(
            catalog.get("openai").unwrap().models,
            compiled_openai().get_models()
        );
    }

    #[test]
    fn an_invalid_provider_base_url_drops_the_whole_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = snapshot_entry(dir.path(), "openai");
        entry["baseUrl"] = "ftp://proxy.example/v1".into();
        serve(dir.path(), serde_json::json!([entry]));

        let catalog = load(dir.path());
        let openai = catalog.get("openai").unwrap();
        let compiled = compiled_openai();
        assert_eq!(openai.base_url, compiled.base_url().map(str::to_string));
        // The entry was dropped whole: its model replacements never applied.
        assert_eq!(openai.models, compiled.get_models());
    }

    #[test]
    fn each_bad_model_field_drops_only_that_model() {
        let dir = tempfile::tempdir().unwrap();
        let entry = snapshot_entry(dir.path(), "openai");
        let compiled_models = entry["models"].as_array().unwrap().clone();
        let first = compiled_models[0].clone();
        let first_id = first["id"].as_str().unwrap().to_string();
        let mut good_replacement = first.clone();
        good_replacement["contextWindow"] = 31_337.into();
        let bad = |id: &str, mutate: &dyn Fn(&mut serde_json::Value)| {
            let mut model = reidentified(&first, id);
            mutate(&mut model);
            model
        };
        let mut missing_required_field = reidentified(&first, "gpt-bad-missing-base-url");
        missing_required_field
            .as_object_mut()
            .unwrap()
            .remove("baseUrl")
            .unwrap();
        let models = serde_json::json!([
            good_replacement,
            reidentified(&first, "gpt-fresh"),
            bad("gpt-bad-url", &|model| {
                model["baseUrl"] = "not-a-url".into();
            }),
            missing_required_field,
            bad("gpt-bad-window", &|model| {
                model["contextWindow"] = 0.into();
            }),
            bad("gpt-bad-cost", &|model| {
                model["cost"]["input"] = (-1.0).into();
            }),
            bad("gpt-bad-tier", &|model| {
                model["cost"]["tiers"] = serde_json::json!([{
                    "input": -5, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                    "inputTokensAbove": 1000
                }]);
            }),
            bad("gpt-bad-api", &|model| {
                model["api"] = "carrier-pigeon".into();
            }),
            bad("gpt-bad-parent", &|model| {
                model["provider"] = "anthropic".into();
            }),
        ]);
        let mut edited = entry.clone();
        edited["models"] = models;
        serve(dir.path(), serde_json::json!([edited]));

        let catalog = load(dir.path());
        let merged = &catalog.get("openai").unwrap().models;
        let position = merged
            .iter()
            .position(|model| model.id == first_id)
            .unwrap();
        // The one valid replacement applied; every bad file-only id vanished
        // while the compiled catalog behind them stayed intact.
        assert_eq!(merged[position].context_window, 31_337);
        assert!(merged.iter().any(|model| model.id == "gpt-fresh"));
        assert!(!merged.iter().any(|model| model.id.starts_with("gpt-bad-")));
        // Every compiled entry survives (replaced or original) plus the one
        // valid file-only addition.
        assert_eq!(merged.len(), compiled_models.len() + 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_created_private_and_a_broader_mode_is_tightened_on_load() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load(dir.path());
        let path = dir.path().join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load(dir.path());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
