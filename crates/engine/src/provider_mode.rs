//! Provider Mode (ADR-provider-mode-is-a-chat-mode): the chat-level catalog-setup mode. The mode
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

use holt_doc::parts::{ChoiceCardState, KeyCardState, MessagePart, ProposalCardState, ProviderRef};
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

/// The Provider Mode toolset (ADR-provider-mode-is-a-chat-mode): web research plus the read-only
/// proposal tool, the Key request, and the provider choice. No file
/// access, no delegation, no MCP, and no apply — the card's Write button
/// is the only write path.
pub(crate) fn provider_mode_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "web_fetch" | "web_search" | "model_proposal" | "request_provider_key" | "choose_provider"
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
         and its providers. Resolve the user's words to ONE concrete provider id — an \
         organization may carry several providers (regions, token plans). Without asking, \
         take the provider already settled earlier in this conversation; else the only one \
         in the organization whose catalog has the named model; else the only configured \
         one. The proposal card names its target provider, so the user sees where it lands \
         and can correct you. When several still match, call `choose_provider` with the \
         candidate ids and STOP — never list them in text. Then call `model_proposal` in \
         inquiry mode (`providerId`, plus `modelId` to dump an existing record as the replacement \
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
/// (ADR-provider-mode-is-a-chat-mode), built from its result details: a proposal card for a stored
/// proposal, a key card for a shown Key request. Inquiries, no-ops, and
/// errors leave none.
pub(crate) fn tool_card(
    tool_call_id: &str,
    tool_name: &str,
    details: &serde_json::Value,
) -> Option<MessagePart> {
    let text = |field: &str| details.get(field)?.as_str().map(str::to_owned);
    let refs = |field: &str| -> Vec<ProviderRef> {
        details
            .get(field)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    };
    match tool_name {
        "model_proposal" => Some(MessagePart::ModelProposal {
            id: format!("{tool_call_id}-card"),
            proposal_id: text("proposalId")?,
            targets: refs("targets"),
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
        "choose_provider" => {
            let options = refs("options");
            (!options.is_empty()).then(|| MessagePart::ProviderChoice {
                id: format!("{tool_call_id}-card"),
                options,
                chosen: None,
                state: ChoiceCardState::Pending,
            })
        }
        _ => None,
    }
}

/// Apply `stamp` to every part in the transcript; each entry it changed
/// re-appends itself to the log (ADR-transcript-append-only-log-per-chat-lock), like `settle_plan_cards`. Card
/// states are display state — the lifecycle already moved in the store or
/// through the RPC. Also the generic pass behind the question card's
/// lifecycle stamps (ADR-ask-user-is-a-question-card).
pub(crate) fn stamp_cards(chat: &ChatRuntime, mut stamp: impl FnMut(&mut MessagePart) -> bool) {
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

/// Retire every pending key card when a newer request lands. A settle that
/// acted on a request stamps only that card ([`stamp_key_card`]); a stale
/// click with no current request uses [`supersede_key_card`] instead.
pub(crate) fn stamp_key_cards(chat: &ChatRuntime, to: KeyCardState) {
    stamp_cards(chat, |part| match part {
        MessagePart::KeyRequest { state, .. } if *state == KeyCardState::Pending => {
            *state = to;
            true
        }
        _ => false,
    });
}

/// Install the newest request. It stays in the slot while its settle is
/// in flight, so another settle and a restart still see it.
pub(crate) fn install_key_request(chat: &ChatRuntime, request: PendingKeyRequest) {
    *chat.key_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(request);
}

/// Why a card's settle could not claim its request.
#[derive(Debug)]
pub(crate) enum KeyRequestClaimError {
    NoPending,
    Replaced,
    InFlight,
}

/// The current pending card for a legacy request that has no card id yet.
/// There is at most one by the key-request invariant; a stale click must
/// never supply the identity for it.
fn pending_card_id(chat: &ChatRuntime, request: &PendingKeyRequest) -> Option<String> {
    let mut found = None;
    for part in chat
        .transcript
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flat_map(|entry| entry.parts.iter())
    {
        let MessagePart::KeyRequest {
            id,
            provider_id,
            destination,
            state,
            ..
        } = part
        else {
            continue;
        };
        if *state == KeyCardState::Pending
            && *provider_id == request.provider_id
            && *destination == request.destination
        {
            if found.is_some() {
                return None;
            }
            found = Some(id.clone());
        }
    }
    found
}

/// Claim the displayed card's current request without taking it out of the
/// slot. Another card may settle concurrently; the same card may not.
pub(crate) fn claim_key_request(
    chat: &ChatRuntime,
    card_id: &str,
    provider_id: &str,
    destination: &str,
) -> Result<PendingKeyRequest, KeyRequestClaimError> {
    let (mut pending, legacy) = {
        let current = chat.key_request.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = current.clone() else {
            let settling = chat
                .key_request_settling
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            return if settling.as_deref() == Some(card_id) {
                Err(KeyRequestClaimError::InFlight)
            } else {
                Err(KeyRequestClaimError::NoPending)
            };
        };
        let legacy = pending.card_id.is_empty();
        (pending, legacy)
    };
    if chat
        .key_request_settling
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(card_id)
    {
        return Err(KeyRequestClaimError::InFlight);
    }
    if pending.provider_id != provider_id || pending.destination != destination {
        return Err(KeyRequestClaimError::Replaced);
    }
    if legacy {
        let actual = pending_card_id(chat, &pending).ok_or(KeyRequestClaimError::Replaced)?;
        if actual != card_id {
            return Err(KeyRequestClaimError::Replaced);
        }
        pending.card_id = actual;
    }
    if pending.card_id != card_id {
        return Err(KeyRequestClaimError::Replaced);
    }

    // Revalidate after resolving a legacy id: a replacement may have landed
    // while the transcript was unlocked.
    let mut current = chat.key_request.lock().unwrap_or_else(|e| e.into_inner());
    let Some(current_request) = current.as_mut() else {
        return Err(KeyRequestClaimError::NoPending);
    };
    if current_request.provider_id != pending.provider_id
        || current_request.destination != pending.destination
        || (!current_request.card_id.is_empty() && current_request.card_id != pending.card_id)
    {
        return Err(KeyRequestClaimError::Replaced);
    }
    current_request.card_id = pending.card_id.clone();
    let mut settling = chat
        .key_request_settling
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if settling.as_deref() == Some(card_id) {
        return Err(KeyRequestClaimError::InFlight);
    }
    *settling = Some(card_id.to_string());
    Ok(pending)
}

/// Finish one card's settle. Only that request is consumed; a replacement
/// stays pending. A failure merely releases the in-flight marker, leaving
/// the request and its card available for retry.
pub(crate) fn finish_key_request_settle(chat: &ChatRuntime, card_id: &str, succeeded: bool) {
    let mut key_request = chat.key_request.lock().unwrap_or_else(|e| e.into_inner());
    let mut settling = chat
        .key_request_settling
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if settling.as_deref() == Some(card_id) {
        *settling = None;
    }
    if succeeded
        && key_request
            .as_ref()
            .is_some_and(|request| request.card_id == card_id)
    {
        *key_request = None;
    }
}

/// Supersede only a card that proved to have no current request. Other
/// pending cards, including one whose settle is in flight, are untouched.
pub(crate) fn supersede_key_card(chat: &ChatRuntime, card_id: &str) {
    stamp_cards(chat, |part| match part {
        MessagePart::KeyRequest { id, state, .. }
            if id == card_id && *state == KeyCardState::Pending =>
        {
            *state = KeyCardState::Superseded;
            true
        }
        _ => false,
    });
}

/// Settle only the card that carried this request. A replacement may have
/// superseded it while its settle was in flight; that does not make the
/// replacement the card to stamp, even when both targets are identical.
pub(crate) fn stamp_key_card(chat: &ChatRuntime, request: &PendingKeyRequest, to: KeyCardState) {
    let mut settled = false;
    stamp_cards(chat, |part| {
        if settled {
            return false;
        }
        let MessagePart::KeyRequest { id, state, .. } = part else {
            return false;
        };
        let mine = id == &request.card_id
            && matches!(*state, KeyCardState::Pending | KeyCardState::Superseded);
        if mine {
            *state = to;
            settled = true;
        }
        mine
    });
}

/// Retire every pending provider-choice card: a newer choice, or any new
/// Turn — a typed answer moved the conversation past it.
pub(crate) fn supersede_choice_cards(chat: &ChatRuntime) {
    stamp_cards(chat, |part| match part {
        MessagePart::ProviderChoice { state, .. } if *state == ChoiceCardState::Pending => {
            *state = ChoiceCardState::Superseded;
            true
        }
        _ => false,
    });
}

/// Settle one pending choice card on `provider_id` — the click — and
/// return the option picked. Checked and stamped under one transcript
/// lock, so two clicks cannot both settle it.
pub(crate) fn choose_provider(
    chat: &ChatRuntime,
    card_id: &str,
    provider_id: &str,
) -> Result<ProviderRef, String> {
    let mut picked = None;
    let mut refusal = "no such provider choice on this chat";
    stamp_cards(chat, |part| match part {
        MessagePart::ProviderChoice {
            id,
            options,
            chosen,
            state,
        } if id == card_id => {
            if *state != ChoiceCardState::Pending {
                refusal = "this provider choice is already settled";
                return false;
            }
            let Some(option) = options.iter().find(|option| option.id == provider_id) else {
                refusal = "the provider is not one of this card's options";
                return false;
            };
            picked = Some(option.clone());
            *chosen = Some(provider_id.to_string());
            *state = ChoiceCardState::Chosen;
            true
        }
        _ => false,
    });
    picked.ok_or_else(|| refusal.to_string())
}

/// Put a chosen card back to pending — the queued message did not go out.
pub(crate) fn unchoose_provider(chat: &ChatRuntime, card_id: &str) {
    stamp_cards(chat, |part| match part {
        MessagePart::ProviderChoice {
            id, chosen, state, ..
        } if id == card_id && *state == ChoiceCardState::Chosen => {
            *chosen = None;
            *state = ChoiceCardState::Pending;
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
            MessagePart::ProviderChoice {
                id, chosen, state, ..
            } if *state == ChoiceCardState::Pending => {
                if let Some(MessagePart::ProviderChoice {
                    chosen: settled_choice,
                    state: settled,
                    ..
                }) = existing.iter().find(|old| old.id() == id.as_str())
                {
                    *chosen = settled_choice.clone();
                    *state = *settled;
                }
            }
            MessagePart::QuestionCard {
                id, answers, state, ..
            } if *state == ChoiceCardState::Pending => {
                if let Some(MessagePart::QuestionCard {
                    answers: settled_answers,
                    state: settled,
                    ..
                }) = existing.iter().find(|old| old.id() == id.as_str())
                {
                    *answers = settled_answers.clone();
                    *state = *settled;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_doc::{MessageRole, SessionMessageEntry};

    fn request(provider_id: &str, destination: &str) -> PendingKeyRequest {
        request_for(provider_id, provider_id, destination)
    }

    fn request_for(card_id: &str, provider_id: &str, destination: &str) -> PendingKeyRequest {
        PendingKeyRequest {
            card_id: card_id.to_string(),
            provider_id: provider_id.to_string(),
            provider_name: provider_id.to_string(),
            destination: destination.to_string(),
        }
    }

    fn card(id: &str, provider_id: &str, destination: &str) -> MessagePart {
        MessagePart::KeyRequest {
            id: id.to_string(),
            provider_id: provider_id.to_string(),
            provider_name: provider_id.to_string(),
            destination: destination.to_string(),
            state: KeyCardState::Pending,
        }
    }

    /// One entry per card, oldest first, as the transcript appends them.
    fn chat_with(cards: Vec<MessagePart>) -> ChatRuntime {
        let chat = ChatRuntime::new();
        *chat.transcript.write().unwrap() = cards
            .into_iter()
            .enumerate()
            .map(|(at, part)| SessionMessageEntry {
                id: format!("entry-{at}"),
                role: MessageRole::Assistant,
                parts: vec![part],
                created_at: 0,
                device_id: "dev".to_string(),
                status: None,
                continuation_of: None,
            })
            .collect();
        chat
    }

    fn states(chat: &ChatRuntime) -> Vec<KeyCardState> {
        chat.transcript
            .read()
            .unwrap()
            .iter()
            .flat_map(|entry| entry.parts.iter())
            .filter_map(|part| match part {
                MessagePart::KeyRequest { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    const ZAI: &str = "https://zai.example/v1";
    const OPENAI: &str = "https://api.openai.example/v1";

    #[test]
    fn a_settle_stamps_its_own_card_and_leaves_a_later_request_s_pending() {
        // Card B's request landed while card A's settle was out.
        let chat = chat_with(vec![card("a", "zai", ZAI), card("b", "openai", OPENAI)]);
        stamp_key_card(&chat, &request_for("a", "zai", ZAI), KeyCardState::Saved);
        assert_eq!(
            states(&chat),
            vec![KeyCardState::Saved, KeyCardState::Pending]
        );
    }

    #[test]
    fn a_settle_stamps_its_own_card_when_the_same_target_lands_mid_settle() {
        // A's replacement has landed: the append superseded A before B
        // became pending. Stamping A must not settle B by target alone.
        let chat = chat_with(vec![card("a", "zai", ZAI), card("b", "zai", ZAI)]);
        stamp_key_cards(&chat, KeyCardState::Superseded);
        let mut transcript = chat.transcript.write().unwrap();
        let MessagePart::KeyRequest { state, .. } = &mut transcript[1].parts[0] else {
            panic!("key card");
        };
        *state = KeyCardState::Pending;
        drop(transcript);
        stamp_key_card(
            &chat,
            &request_for("a", "zai", ZAI),
            KeyCardState::Dismissed,
        );
        assert_eq!(
            states(&chat),
            vec![KeyCardState::Dismissed, KeyCardState::Pending]
        );
    }

    #[test]
    fn a_settled_card_is_not_stamped_again() {
        let chat = chat_with(vec![card("a", "zai", ZAI)]);
        stamp_key_card(&chat, &request_for("a", "zai", ZAI), KeyCardState::Saved);
        stamp_key_card(
            &chat,
            &request_for("a", "zai", ZAI),
            KeyCardState::Dismissed,
        );
        assert_eq!(states(&chat), vec![KeyCardState::Saved]);
    }

    #[test]
    fn a_newer_request_still_retires_every_pending_card() {
        let chat = chat_with(vec![card("a", "zai", ZAI), card("b", "openai", OPENAI)]);
        stamp_key_cards(&chat, KeyCardState::Superseded);
        assert_eq!(
            states(&chat),
            vec![KeyCardState::Superseded, KeyCardState::Superseded]
        );
    }

    #[test]
    fn a_finish_consumes_only_its_own_request_and_failure_keeps_it() {
        // With no replacement, failure leaves the request retryable.
        let vacant = ChatRuntime::new();
        install_key_request(&vacant, request("zai", ZAI));
        *vacant.key_request_settling.lock().unwrap() = Some("zai".into());
        finish_key_request_settle(&vacant, "zai", false);
        assert_eq!(
            *vacant.key_request.lock().unwrap(),
            Some(request("zai", ZAI))
        );
        assert_eq!(*vacant.key_request_settling.lock().unwrap(), None);

        // B has landed and its own settle is in flight. A's completion
        // consumes nothing; B's failure leaves B; only B's success consumes it.
        let in_flight = ChatRuntime::new();
        install_key_request(&in_flight, request("zai", ZAI));
        *in_flight.key_request_settling.lock().unwrap() = Some("zai".into());
        install_key_request(&in_flight, request("openai", OPENAI));
        *in_flight.key_request_settling.lock().unwrap() = Some("openai".into());
        finish_key_request_settle(&in_flight, "zai", true);
        assert_eq!(
            *in_flight.key_request.lock().unwrap(),
            Some(request("openai", OPENAI))
        );
        finish_key_request_settle(&in_flight, "openai", false);
        assert_eq!(
            *in_flight.key_request.lock().unwrap(),
            Some(request("openai", OPENAI))
        );
        finish_key_request_settle(&in_flight, "openai", true);
        assert_eq!(*in_flight.key_request.lock().unwrap(), None);
    }

    #[test]
    fn a_legacy_settle_reads_the_actual_pending_card_not_the_clicked_id() {
        let chat = chat_with(vec![card("a", "zai", ZAI), card("b", "zai", ZAI)]);
        stamp_key_cards(&chat, KeyCardState::Superseded);
        let mut transcript = chat.transcript.write().unwrap();
        let MessagePart::KeyRequest { state, .. } = &mut transcript[1].parts[0] else {
            panic!("key card");
        };
        *state = KeyCardState::Pending;
        drop(transcript);
        let mut legacy = request_for("b", "zai", ZAI);
        legacy.card_id.clear();
        install_key_request(&chat, legacy);

        assert!(matches!(
            claim_key_request(&chat, "a", "zai", ZAI),
            Err(KeyRequestClaimError::Replaced)
        ));
        let claimed = claim_key_request(&chat, "b", "zai", ZAI).unwrap();
        assert_eq!(claimed.card_id, "b");
        assert_eq!(
            chat.key_request.lock().unwrap().as_ref().unwrap().card_id,
            "b"
        );
    }

    #[test]
    fn a_duplicate_settle_changes_neither_the_request_nor_its_card() {
        let chat = chat_with(vec![card("b", "openai", OPENAI)]);
        install_key_request(&chat, request_for("b", "openai", OPENAI));
        *chat.key_request_settling.lock().unwrap() = Some("b".into());
        assert!(matches!(
            claim_key_request(&chat, "b", "openai", OPENAI),
            Err(KeyRequestClaimError::InFlight)
        ));
        assert_eq!(states(&chat), vec![KeyCardState::Pending]);
        finish_key_request_settle(&chat, "b", false);
        assert_eq!(states(&chat), vec![KeyCardState::Pending]);
        assert!(
            claim_key_request(&chat, "b", "openai", OPENAI).is_ok(),
            "the failed request can be settled again"
        );
    }

    #[test]
    fn an_in_flight_settle_stays_durable_and_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let mut chat = chat_with(vec![card("b", "openai", OPENAI)]);
        chat.data_dir = directory.path().to_path_buf();
        chat.chat_id = "chat-1".into();
        install_key_request(&chat, request_for("b", "openai", OPENAI));
        claim_key_request(&chat, "b", "openai", OPENAI).unwrap();
        chat.save_provider_mode();

        let restored = ChatRuntime::load(directory.path(), "chat-1", "dev");
        assert_eq!(
            *restored.key_request.lock().unwrap(),
            Some(request_for("b", "openai", OPENAI))
        );
        assert_eq!(*restored.key_request_settling.lock().unwrap(), None);
    }
}
