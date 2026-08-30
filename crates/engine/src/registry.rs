//! Harness catalog types for the shell.
//!
//! The harness drivers (claude-code, codex, acp, opencode, cursor, mock) were
//! removed with the engine; these descriptor types remain because the settings
//! and picker UI reads them. [`descriptors`] serves an empty catalog until a
//! real backend replaces it.

use holt_proto::{HarnessId, ReasoningLevel, SteeringMode};
use serde::{Deserialize, Serialize};

/// What `ListHarnesses` reports per harness. Same wire shape the UI already
/// parses (camelCase).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// Whether the agent's CLI is present on the listing device.
    #[serde(default = "default_installed")]
    pub installed: bool,
    /// Whether the listing device offers this harness (Settings → Agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn default_installed() -> bool {
    true
}

/// A descriptor's effective enabled flag. `None` falls back to `installed`.
pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor.enabled.unwrap_or(descriptor.installed)
}

/// The stub catalog: no harnesses. A real backend serves its own list here.
pub fn descriptors() -> Vec<HarnessDescriptor> {
    Vec::new()
}
