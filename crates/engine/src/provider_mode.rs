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

use holt_doc::parts::{KeyCardState, MessagePart, ProposalCardState};
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

/// The Provider Mode toolset (ADR-0037): web research plus the read-only
/// proposal tool and the Key request. No file access, no delegation, no
/// MCP, and no apply — the card's Write button is the only write path.
pub(crate) fn provider_mode_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "web_fetch" | "web_search" | "model_proposal" | "request_provider_key"
    )
}

/// The Provider Mode system-prompt block, appended to the chat's ordinary
/// prompt: the fixed research → resolve → propose → stop workflow.
pub(crate) fn provider_mode_block(web_search: bool) -> String {
    let search = if web_search {
        "`web_search` to find pages, "
    } else {
        ""
    };
    format!(
        "## Provider Mode (active)\n\n\
         This Turn runs in Provider Mode: your only job is preparing provider and model \
         catalog changes. You have no file access and cannot write the catalog — the user \
         writes a proposal with the Write button on its card. Follow this procedure:\n\
         1. Research: use {search}`web_fetch` to read the provider's official docs — base \
         URL, API style, model IDs, context window, max output tokens, input modalities, \
         reasoning levels, pricing.\n\
         2. Resolve: call `model_proposal` with no `providerId` to list every organization \
         and its providers. Match the user's words to ONE concrete provider id — an \
         organization may carry several providers (regions, token plans); when several \
         match, show them and ASK which one. Then call `model_proposal` in inquiry mode \
         (`providerId`, plus `modelId` to dump an existing record as the replacement \
         template) to see the local catalog and detect no-ops. A provider that does not \
         exist yet is addressed as a draft `provider` object `{{id, name, baseUrl, \
         defaultApi}}` for the inquiry probe and the key request.\n\
         3. Keys: as soon as the docs say the endpoint needs authentication — or a probe \
         fails with HTTP 401/403 — call `request_provider_key` once and stop. A key card \
         appears in the conversation; the user saves the key there and your next message \
         reports the outcome. A saved key means re-run the probe. Keys are never typed in \
         chat, and you never ask for one in text.\n\
         4. Propose: call `model_proposal` with `changes` — complete records; copy \
         api/compat/thinkingLevelMap from the dump when replacing an id and change only \
         what differs; use set_hidden_models for retired ids. A new proposal touching the \
         same provider replaces the previous one.\n\
         5. Stop: give a short summary and STOP. A proposal card appears in the \
         conversation; the user reviews and writes it there. Never claim a proposal is \
         written or applied, and do not ask for approval in chat.\n\
         If the docs lack a field you need, say exactly what is missing and ask — never \
         guess a model ID or a price."
    )
}

/// The card a settled catalog tool call leaves in the transcript
/// (ADR-0037), built from its result details: a proposal card for a stored
/// proposal, a key card for a shown Key request. Inquiries, no-ops, and
/// errors leave none.
pub(crate) fn tool_card(
    tool_call_id: &str,
    tool_name: &str,
    details: &serde_json::Value,
) -> Option<MessagePart> {
    let text = |field: &str| details.get(field)?.as_str().map(str::to_owned);
    match tool_name {
        "model_proposal" => Some(MessagePart::ModelProposal {
            id: format!("{tool_call_id}-card"),
            proposal_id: text("proposalId")?,
            summary: text("summary").unwrap_or_default(),
            lines: details
                .get("lines")
                .and_then(serde_json::Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| line.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            state: ProposalCardState::Pending,
        }),
        "request_provider_key" => Some(MessagePart::KeyRequest {
            id: format!("{tool_call_id}-card"),
            provider_id: text("providerId")?,
            provider_name: text("providerName").unwrap_or_default(),
            destination: text("destination").unwrap_or_default(),
            state: KeyCardState::Pending,
        }),
        _ => None,
    }
}

/// Apply `stamp` to every part in the transcript; each entry it changed
/// re-appends itself to the log (ADR-0032), like `settle_plan_cards`. Card
/// states are display state — the lifecycle already moved in the store or
/// through the RPC.
fn stamp_cards(chat: &ChatRuntime, mut stamp: impl FnMut(&mut MessagePart) -> bool) {
    let mut transcript = chat
        .transcript
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let mut changed = Vec::new();
    for entry in transcript.iter_mut() {
        let mut entry_changed = false;
        for part in entry.parts.iter_mut() {
            entry_changed |= stamp(part);
        }
        if entry_changed {
            changed.push(entry.id.clone());
        }
    }
    drop(transcript);
    for entry_id in changed {
        chat.persist_entry(&entry_id);
    }
}

/// Settle the pending card for one proposal.
pub(crate) fn stamp_proposal_card(chat: &ChatRuntime, proposal: &str, to: ProposalCardState) {
    stamp_cards(chat, |part| match part {
        MessagePart::ModelProposal {
            proposal_id, state, ..
        } if proposal_id == proposal && *state == ProposalCardState::Pending => {
            *state = to;
            true
        }
        _ => false,
    });
}

/// Supersede every pending proposal card whose proposal is no longer
/// stored — replaced by a newer proposal on the same provider, or evicted
/// by the cap — so no card offers a Write that can only fail.
pub(crate) fn supersede_orphan_proposal_cards(chat: &ChatRuntime) {
    let stored: HashSet<String> = chat
        .proposals
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .map(|proposal| proposal.id.clone())
        .collect();
    stamp_cards(chat, |part| match part {
        MessagePart::ModelProposal {
            proposal_id, state, ..
        } if *state == ProposalCardState::Pending && !stored.contains(proposal_id) => {
            *state = ProposalCardState::Superseded;
            true
        }
        _ => false,
    });
}

/// Settle every pending key card: at most one Key request is pending per
/// chat, so a settle or a newer request retires all of them.
pub(crate) fn stamp_key_cards(chat: &ChatRuntime, to: KeyCardState) {
    stamp_cards(chat, |part| match part {
        MessagePart::KeyRequest { state, .. } if *state == KeyCardState::Pending => {
            *state = to;
            true
        }
        _ => false,
    });
}

/// Keep settled card states across a live entry rewrite: a running Turn
/// rebuilds its entry from its own part list, which still holds the card
/// as it was appended, while the RPC may already have stamped it.
pub(crate) fn carry_card_states(existing: &[MessagePart], parts: &mut [MessagePart]) {
    for part in parts.iter_mut() {
        match part {
            MessagePart::ModelProposal { id, state, .. }
                if *state == ProposalCardState::Pending =>
            {
                if let Some(MessagePart::ModelProposal { state: settled, .. }) =
                    existing.iter().find(|old| old.id() == id.as_str())
                {
                    *state = *settled;
                }
            }
            MessagePart::KeyRequest { id, state, .. } if *state == KeyCardState::Pending => {
                if let Some(MessagePart::KeyRequest { state: settled, .. }) =
                    existing.iter().find(|old| old.id() == id.as_str())
                {
                    *state = *settled;
                }
            }
            _ => {}
        }
    }
}
