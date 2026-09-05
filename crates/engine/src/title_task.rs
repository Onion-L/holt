//! The one-shot Title task (ADR-0012): when a chat's first user prompt is
//! accepted, one independent background request asks the configured title
//! model for a better name. The task is deliberately outside the Turn
//! lifecycle — its own cancellation token (only chat deletion cancels it),
//! no Session status, no History/Transcript/preview/usage footprint, no
//! retries. Every failure is silent and preserves the first-line fallback.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use holt_proto::TitleSource;
use pi_core::agent::types::StreamFn;
use pi_core::ai::types::{
    BlockContent, CacheRetention, Context, Message, Model, RoleUser, SimpleStreamOptions,
    StopReason, StreamOptions, TextContent, UserContent, UserMessage,
};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentRuntime;
use crate::rpc::TITLE_CHAR_LIMIT;
use crate::store::persist_chats;

/// A hung title request becomes a silent failure after this long — the task
/// is auxiliary, so it never waits on a provider forever.
const TITLE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A title is a few words; cap the reply budget well under any model's floor.
const TITLE_MAX_TOKENS: u64 = 128;

/// Everything the task needs, resolved at first-prompt acceptance so a
/// misconfigured title model can never fail the Turn that triggered it.
pub(crate) struct TitleTaskSpec {
    pub(crate) chat_id: String,
    pub(crate) data_dir: PathBuf,
    /// The first user prompt — the only content the title model ever sees.
    pub(crate) prompt: String,
    pub(crate) instruction: String,
    pub(crate) model: Model,
    pub(crate) api_key: String,
    pub(crate) stream_fn: Option<StreamFn>,
}

pub(crate) async fn run_title_task(
    runtime: Arc<AgentRuntime>,
    spec: TitleTaskSpec,
    generation: DateTime<Utc>,
    cancel: CancellationToken,
) {
    let Some(title) = complete_title(&spec, &cancel).await else {
        return;
    };
    // Write-back guard: the chat must still exist, still be this chat
    // instance (delete + recreate under the same id fails the generation
    // check), and still be automatically titled — a manual rename, even to
    // identical text, flips the source and always wins.
    let _persistence = runtime
        .persistence
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if runtime.stopping.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut chats = runtime.chats.write().unwrap_or_else(|e| e.into_inner());
    let Some(row) = chats.iter_mut().find(|row| row.id == spec.chat_id) else {
        return;
    };
    if row.created_at != generation || row.title_source != TitleSource::Automatic {
        return;
    }
    row.title = Some(title);
    drop(chats);
    if persist_chats(
        &spec.data_dir,
        &runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
    )
    .is_err()
    {
        return;
    }
    runtime.publish_chats();
}

/// One completion against the title model: the configured instruction as
/// the system prompt, the first user prompt as the only message, no tools.
/// Returns `None` on every failure — provider errors, aborts, cancellation,
/// timeouts, and unusable output all keep the fallback quietly.
async fn complete_title(spec: &TitleTaskSpec, cancel: &CancellationToken) -> Option<String> {
    let max_tokens = if spec.model.max_tokens > 0 {
        spec.model.max_tokens.min(TITLE_MAX_TOKENS)
    } else {
        TITLE_MAX_TOKENS
    };
    let mut options = SimpleStreamOptions {
        base: StreamOptions {
            max_tokens: Some(max_tokens),
            cache_retention: Some(CacheRetention::None),
            base: pi_core::ai::types::ProviderRequestOptions {
                signal: Some(cancel.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        reasoning: None,
        ..Default::default()
    };
    options.base.base.api_key = Some(spec.api_key.clone());
    let context = Context {
        system_prompt: Some(spec.instruction.clone()),
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: spec.prompt.clone(),
                ..Default::default()
            })]),
            timestamp: 0,
        })],
        tools: None,
    };
    let stream_fn = spec
        .stream_fn
        .clone()
        .unwrap_or_else(crate::agent::default_stream_fn);
    let stream = stream_fn(&spec.model, &context, Some(&options)).ok()?;
    // Race the consume against the token — the scripted seam's never-ending
    // streams can only be cancelled this way, and a real transport sees the
    // signal in the options too — plus the auxiliary-request timeout.
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);
    let consume = async {
        while let Some(event) = stream.next().await {
            if event.is_terminal() {
                break;
            }
        }
    };
    tokio::select! {
        _ = consume => {}
        _ = &mut cancelled => return None,
        _ = tokio::time::sleep(TITLE_REQUEST_TIMEOUT) => return None,
    }
    let response = stream.result().await;
    match response.stop_reason {
        StopReason::Aborted | StopReason::Error => None,
        _ => normalize_title(&pi_core::ai::utils::text::content_text(
            &response.content,
            "\n",
        )),
    }
}

/// Trim, flatten line breaks to spaces, reject empties, cap at the sidebar
/// title ceiling.
fn normalize_title(text: &str) -> Option<String> {
    let flattened = text.lines().collect::<Vec<_>>().join(" ");
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(TITLE_CHAR_LIMIT).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_trims_flattens_and_caps() {
        assert_eq!(
            normalize_title("  first line\nsecond line  "),
            Some("first line second line".to_string())
        );
        assert_eq!(
            normalize_title(&"y".repeat(100)),
            Some("y".repeat(TITLE_CHAR_LIMIT))
        );
        assert_eq!(
            normalize_title("windows\r\nline"),
            Some("windows line".to_string())
        );
    }

    #[test]
    fn normalization_rejects_empty_output() {
        assert_eq!(normalize_title(""), None);
        assert_eq!(normalize_title("   \n  \n "), None);
    }
}
