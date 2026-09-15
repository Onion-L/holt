//! Boot-time snapshot of the built-in provider catalog
//! (`<data_dir>/provider-store.json`): one entry per `builtin_providers()`
//! element carrying the fields the `Provider` trait exposes, with that
//! provider's `get_models()` output nested verbatim as
//! `pi_core::ai::types::Model`.
//!
//! Write-only for now: nothing reads the file back — provider and model
//! resolution still goes straight to `pi-core-rs`. The compiled catalog is
//! the source of truth, so the file is rewritten on every boot rather than
//! created once, and it is a derived artifact: existing content, malformed
//! included, is simply overwritten, and a failed write is logged by the
//! caller instead of failing engine assembly (unlike
//! `provider-credentials.json` and `provider-settings.json`, whose
//! corruption is a boot gate).
//!
//! The source is `builtin_providers()` plus each provider's own
//! `get_models()` — never `ProviderAdapter`, whose `Models` instance carries
//! injected credentials and whose projection adds Holt-owned custom model
//! ids. Holt's availability policy (`eligible_providers()`) is deliberately
//! not applied here: the file is provider data, filtered at read time.

use std::{io::Write, path::Path};

use pi_core::ai::{
    providers::builtin::{builtin_providers, get_builtin_model_data_generated_at},
    types::{Model as CoreModel, ProviderHeaders},
};
use serde::Serialize;

use crate::EngineError;

const FILE_NAME: &str = "provider-store.json";

/// Envelope version. A later shape change bumps it so readers can detect the
/// difference instead of guessing.
const VERSION: u32 = 1;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderStore {
    version: u32,
    /// The compiled catalog's own generation stamp, or `null` when
    /// `pi-core-rs` reports none.
    source_generated_at: Option<i64>,
    written_at: i64,
    providers: Vec<ProviderEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderEntry {
    id: String,
    name: String,
    /// `null` when the provider stands alone.
    organization_id: Option<String>,
    /// `null` when the provider declares none.
    base_url: Option<String>,
    /// The trait value verbatim — static provider configuration, not
    /// credential material.
    headers: Option<ProviderHeaders>,
    /// The shape of the requirement, never the material.
    auth: AuthShape,
    /// In `get_models()` order: the catalog order is the provider's own, so
    /// nothing is sorted, deduplicated, or filtered here.
    models: Vec<CoreModel>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthShape {
    api_key: bool,
    oauth: bool,
}

/// Serializes the current built-in catalog and replaces
/// `<data_dir>/provider-store.json` atomically: a temp file in the data dir,
/// then `std::fs::rename` (which replaces an existing destination on Unix and
/// Windows alike). No explicit file mode — the file holds no secrets.
pub(crate) fn write(data_dir: &Path) -> Result<(), EngineError> {
    let bytes = serde_json::to_vec_pretty(&snapshot())
        .map_err(|error| EngineError::Other(error.to_string()))?;
    let path = data_dir.join(FILE_NAME);
    let temp = data_dir.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(EngineError::Io)
}

fn snapshot() -> ProviderStore {
    let providers = builtin_providers()
        .into_iter()
        .map(|provider| ProviderEntry {
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
        .collect();
    ProviderStore {
        version: VERSION,
        source_generated_at: get_builtin_model_data_generated_at(),
        written_at: chrono::Utc::now().timestamp_millis(),
        providers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(data_dir: &Path) -> serde_json::Value {
        write(data_dir).unwrap();
        serde_json::from_slice(&std::fs::read(data_dir.join(FILE_NAME)).unwrap()).unwrap()
    }

    fn optional(value: Option<&str>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, serde_json::Value::from)
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
}
