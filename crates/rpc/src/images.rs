//! Local image persistence and preview contracts. Payloads contain base64 bytes.
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePathParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageImageParams {
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageData {
    pub mime_type: String,
    pub data: String,
}

/// `ReadWorkspaceImage` reply: the validated image bytes plus the
/// engine-resolved canonical path (symlinks resolved), so the opening tab's
/// alias identity matches a text read's.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceImageData {
    pub path: String,
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedImage {
    pub path: String,
    pub mime_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReleaseImageResult {
    pub released: bool,
}
