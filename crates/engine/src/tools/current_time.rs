//! The main-chat `current_time` tool: the pull channel for clock
//! knowledge (ADR-model-facing-time-is-pulled-not-embedded). Tool
//! descriptions and the system prompt sit ahead of the messages in the
//! provider's cached prefix, so a byte that changes between Turns re-keys
//! the cache and re-bills the whole conversation as fresh input. The
//! clock therefore reaches the model through a call, never through the
//! prefix: the description is a static contract, and the answer is
//! computed at call time from the engine clock.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use holt_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{Clock, routines::local_time_zone};

const DESCRIPTION: &str = "Returns the device's time zone, the local wall-clock time, and the UTC \
time right now. Call this before resolving anything that needs 'now' — a wall-clock schedule such \
as 'tomorrow at 9', a date computation, an elapsed check — nothing else in the context states the \
current time or zone.";

/// The answer text, split out for the clock-fixed unit test. An unreadable
/// zone string (the resolver already falls back to "UTC") degrades to UTC,
/// never to an error.
fn result_text(now: DateTime<Utc>, device_zone: &str) -> String {
    let local = match device_zone.parse::<chrono_tz::Tz>() {
        Ok(zone) => now
            .with_timezone(&zone)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    };
    format!(
        "Device time zone: {device_zone}\nLocal time: {local}\nUTC: {}",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

pub(crate) fn create_current_time_tool(clock: Clock) -> AgentTool {
    AgentTool {
        name: "current_time".into(),
        label: "Current Time".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  _params: &serde_json::Value,
                  signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let result = if signal.is_some_and(CancellationToken::is_cancelled) {
                    Err("current_time cancelled".into())
                } else {
                    let text = result_text(clock.now(), &local_time_zone());
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text,
                            ..Default::default()
                        })],
                        ..Default::default()
                    })
                };
                Box::pin(async move { result })
                    as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-07T09:00:00Z".parse().unwrap()
    }

    #[test]
    fn the_answer_reads_local_not_utc() {
        // 09:00Z is 17:00 in Asia/Shanghai — the same wrong-hour trap the
        // old embedded-time fix was written for, now on the call path.
        let text = result_text(now(), "Asia/Shanghai");
        assert!(text.contains("Device time zone: Asia/Shanghai"), "{text}");
        assert!(text.contains("Local time: 2026-10-07 17:00:00"), "{text}");
        assert!(text.contains("UTC: 2026-10-07T09:00:00Z"), "{text}");
    }

    #[test]
    fn an_unreadable_zone_degrades_to_utc() {
        let text = result_text(now(), "Not/AZone");
        assert!(
            text.contains("Local time: 2026-10-07 09:00:00 UTC"),
            "{text}"
        );
    }

    #[test]
    fn the_description_is_a_static_contract() {
        // The description must never carry a clock: tools live in the
        // cached prefix ahead of the conversation, so any time-varying
        // byte re-keys the whole cache. A wall-clock time cannot appear
        // without a digit:digit window.
        assert!(
            !DESCRIPTION
                .chars()
                .collect::<Vec<_>>()
                .windows(3)
                .any(|w| w[0].is_ascii_digit() && w[1] == ':' && w[2].is_ascii_digit()),
            "{DESCRIPTION}"
        );
    }
}
