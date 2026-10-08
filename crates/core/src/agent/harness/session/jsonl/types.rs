//! Port of `pi-core/agent/src/harness/session/jsonl/types.ts`.
//!
//! The TypeScript structural `Pick<FileSystem, ...>` narrows the filesystem
//! surface the repo needs; the Rust port takes the full
//! [`crate::agent::harness::types::FileSystem`] trait object.

use super::super::types::SessionMetadata;

/// Port of `JsonlSessionRepoOptions`.
pub struct JsonlSessionRepoOptions {
    pub fs: std::sync::Arc<dyn crate::agent::harness::types::FileSystem>,
    /// Root containing coding-agent-compatible cwd-encoded session
    /// directories.
    pub sessions_root: String,
}

/// Port of `JsonlSessionMetadata`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlSessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    pub cwd: String,
    pub path: String,
    /// Filesystem modification time as milliseconds since Unix epoch.
    pub modified_at: f64,
    pub source_format: u8,
    #[serde(
        rename = "parentSessionId",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub parent_session_id: Option<String>,
    /// Present only when a v3 parent path could not be resolved to a
    /// session id.
    #[serde(
        rename = "legacyParentSessionPath",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub legacy_parent_session_path: Option<String>,
    /// Opaque application-owned metadata.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
}

impl From<JsonlSessionMetadata> for SessionMetadata {
    fn from(metadata: JsonlSessionMetadata) -> SessionMetadata {
        SessionMetadata {
            id: metadata.id,
            created_at: metadata.created_at,
            parent_session_id: metadata.parent_session_id,
        }
    }
}

/// Port of `JsonlSessionCreateOptions`.
#[derive(Clone, Debug, Default)]
pub struct JsonlSessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
    pub cwd: String,
    pub metadata: Option<serde_json::Value>,
}

/// Port of `JsonlSessionListOptions`.
#[derive(Clone, Debug, Default)]
pub struct JsonlSessionListOptions {
    pub cwd: Option<String>,
}

/// Port of `JsonlV4Header`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlV4Header {
    pub version: u8,
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    pub cwd: String,
    #[serde(
        rename = "parentSessionId",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub parent_session_id: Option<String>,
    #[serde(
        rename = "legacyParentSessionPath",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub legacy_parent_session_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
}

impl JsonlV4Header {
    /// The literal `kind: "header"` marker emitted first on the wire.
    pub const KIND: &'static str = "header";
}
