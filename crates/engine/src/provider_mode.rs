//! Provider Mode (ADR-0037): the chat-level catalog-setup mode. The mode
//! flag rides the chat row (`Chat::provider_mode`); this module owns the
//! chat's persisted setup state — stored proposals, the pending Key
//! request, and the approved key destinations — so a restart keeps every
//! pending card actionable. Nothing here holds a key or a secret header:
//! proposals carry parsed changes (headers are rejected) and a baseline
//! hash, the Key request only where a key would go.

use std::{
    collections::{HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    agent::ChatRuntime,
    tools::model_setup::{PendingKeyRequest, StoredProposal},
};

const DIR_NAME: &str = "provider-mode";

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderModeFile {
    #[serde(default)]
    proposals: Vec<StoredProposal>,
    #[serde(default)]
    key_request: Option<PendingKeyRequest>,
    /// (providerId, baseUrl) pairs, sorted for a stable file.
    #[serde(default)]
    approved_key_destinations: Vec<(String, String)>,
}

/// The chat's setup state as loaded: what `ChatRuntime::load` seeds.
#[derive(Default)]
pub(crate) struct ProviderModeState {
    pub(crate) proposals: VecDeque<StoredProposal>,
    pub(crate) key_request: Option<PendingKeyRequest>,
    pub(crate) approved_key_destinations: HashSet<(String, String)>,
}

fn state_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    crate::store::id_is_path_safe(chat_id)
        .then(|| data_dir.join(DIR_NAME).join(format!("{chat_id}.json")))
}

/// Reads the chat's state file. Missing is the common case (a chat that
/// never entered the mode); a corrupt file starts empty — the model can
/// simply propose again.
pub(crate) fn load(data_dir: &Path, chat_id: &str) -> ProviderModeState {
    let Some(path) = state_path(data_dir, chat_id) else {
        return ProviderModeState::default();
    };
    let file = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<ProviderModeFile>(&bytes) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(target: "holt::provider_mode", %error, chat_id, "unreadable provider-mode state; starting empty");
                ProviderModeFile::default()
            }
        },
        Err(_) => ProviderModeFile::default(),
    };
    ProviderModeState {
        proposals: file.proposals.into(),
        key_request: file.key_request,
        approved_key_destinations: file.approved_key_destinations.into_iter().collect(),
    }
}

/// Writes the chat's current state atomically; an empty state removes the
/// file. Ephemeral (test) and removed chats write nothing.
pub(crate) fn save(chat: &ChatRuntime) {
    if chat.chat_id.is_empty() || chat.is_removed() {
        return;
    }
    let Some(path) = state_path(&chat.data_dir, &chat.chat_id) else {
        return;
    };
    let mut approved: Vec<(String, String)> = chat
        .approved_key_destinations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect();
    approved.sort();
    let file = ProviderModeFile {
        proposals: chat
            .proposals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect(),
        key_request: chat
            .key_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        approved_key_destinations: approved,
    };
    if file == ProviderModeFile::default() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Err(error) = write_atomic(&path, &file) {
        tracing::warn!(target: "holt::provider_mode", %error, chat_id = chat.chat_id, "provider-mode state not saved");
    }
}

fn write_atomic(path: &Path, file: &ProviderModeFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    let parent = path.parent().expect("state path has a parent");
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Drops the chat's state file (chat deletion).
pub(crate) fn delete(data_dir: &Path, chat_id: &str) {
    if let Some(path) = state_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
}
