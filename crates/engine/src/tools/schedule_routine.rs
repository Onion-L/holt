//! The main-chat `schedule_routine` tool: create a persisted Routine from a
//! self-contained prompt and a relative, absolute, or cron schedule. The
//! current Chat supplies the model, permission mode, and Space; the ordinary
//! Routine scheduler owns execution after the write.

use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use futures::future::BoxFuture;
use holt_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use holt_proto::RoutineCheckout;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::AgentRuntime,
    routines::{RoutineDraft, Routines, next_fire, time_zone_or_local},
};

const DESCRIPTION: &str = "Create a Holt Routine for a task that must happen later or recur. Use this \
instead of Bash `sleep`, background processes, or a hand-written timer script. The prompt must be \
self-contained because the Routine starts a fresh Chat without this conversation: include every \
necessary instruction, repository, issue, and URL. The new Routine inherits the current Chat's model, \
permission mode, and Space. Choose exactly one schedule: `delayMinutes` for relative requests such \
as 'in two hours' (stored as an absolute UTC instant), `at` for one local wall-clock time in \
`timeZone`, or `cron` for recurrence. `timeZone` is an IANA name and defaults to the device zone. \
The tool returns the persisted routine \
id and its actual nextFireAt; relay that time in your answer.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolInput {
    name: String,
    prompt: String,
    delay_minutes: Option<i64>,
    at: Option<NaiveDateTime>,
    cron: Option<String>,
    time_zone: Option<String>,
}

struct ScheduleSpec {
    cron: String,
    at: Option<NaiveDateTime>,
    time_zone: String,
}

impl ScheduleSpec {
    fn from_input(input: &ToolInput, now: DateTime<Utc>) -> Result<Self, String> {
        let selected = usize::from(input.delay_minutes.is_some())
            + usize::from(input.at.is_some())
            + usize::from(input.cron.is_some());
        if selected != 1 {
            return Err("pass exactly one of delayMinutes, at, or cron".into());
        }
        if let Some(minutes) = input.delay_minutes {
            if minutes <= 0 {
                return Err("delayMinutes must be greater than zero".into());
            }
            let delay = chrono::Duration::try_minutes(minutes)
                .ok_or("delayMinutes is too large".to_string())?;
            let fire = now
                .checked_add_signed(delay)
                .ok_or("delayMinutes is too large".to_string())?;
            return Ok(Self {
                cron: String::new(),
                at: Some(fire.naive_utc()),
                time_zone: "UTC".into(),
            });
        }
        let time_zone = time_zone_or_local(input.time_zone.clone());
        Ok(Self {
            cron: input.cron.clone().unwrap_or_default(),
            at: input.at,
            time_zone,
        })
    }
}

fn current_chat_target(
    runtime: &AgentRuntime,
    chat_id: &str,
) -> Result<(holt_proto::ChatConfig, String), String> {
    let chats = runtime.chats.read().unwrap_or_else(|e| e.into_inner());
    let chat = chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .ok_or("the current chat was deleted")?;
    let config = chat
        .config
        .clone()
        .ok_or("the current chat has no model config")?;
    let space_id = chat
        .space_id
        .clone()
        .ok_or("the current chat has no Space")?;
    Ok((config, space_id))
}

fn schedule_routine(
    runtime: &AgentRuntime,
    routines: &Routines,
    chat_id: &str,
    tool_call_id: &str,
    input: ToolInput,
) -> Result<AgentToolResult, String> {
    let schedule = ScheduleSpec::from_input(&input, routines.now())?;
    // deleteSpace uses the same registry lock: creation either lands before
    // its pause sweep, or observes the deleted Chat and writes nothing.
    let _chats_store = runtime
        .chats_store
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (config, space_id) = current_chat_target(runtime, chat_id)?;
    let routine = routines
        .create(RoutineDraft {
            id: Some(format!("agent-{chat_id}-{tool_call_id}")),
            name: input.name,
            space_id,
            prompt: input.prompt,
            cron: schedule.cron,
            at: schedule.at,
            time_zone: schedule.time_zone,
            config,
            checkout: RoutineCheckout::MainCheckout,
        })
        .map_err(|error| error.to_string())?;
    let next_fire_at = next_fire(&routine);
    let acknowledgement = match next_fire_at {
        Some(at) => format!(
            "Scheduled routine {:?} ({}). Next fire: {}.",
            routine.name,
            routine.id,
            at.to_rfc3339()
        ),
        None => format!(
            "Created routine {:?} ({}), but it has no future fire.",
            routine.name, routine.id
        ),
    };
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: acknowledgement,
            ..Default::default()
        })],
        details: json!({
            "routineId": routine.id,
            "nextFireAt": next_fire_at,
        }),
        ..Default::default()
    })
}

pub(crate) fn create_schedule_routine_tool(
    runtime: Arc<AgentRuntime>,
    routines: Arc<Routines>,
    chat_id: String,
) -> AgentTool {
    AgentTool {
        name: "schedule_routine".into(),
        label: "Schedule Routine".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "A short name for the routine"
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete, self-contained prompt for the future fresh Chat"
                },
                "delayMinutes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Fire once this many minutes from now"
                },
                "at": {
                    "type": "string",
                    "description": "One local wall-clock time such as 2026-10-08T09:00:00, read in timeZone"
                },
                "cron": {
                    "type": "string",
                    "description": "A five-field cron expression for recurrence"
                },
                "timeZone": {
                    "type": "string",
                    "description": "An IANA time zone such as Asia/Shanghai; defaults to the device zone"
                }
            },
            "required": ["name", "prompt"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let result = if signal.is_some_and(CancellationToken::is_cancelled) {
                    Err("schedule_routine cancelled".into())
                } else {
                    serde_json::from_value::<ToolInput>(params.clone())
                        .map_err(|error| format!("invalid schedule_routine parameters: {error}"))
                        .and_then(|input| {
                            schedule_routine(&runtime, &routines, &chat_id, tool_call_id, input)
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

    fn input(value: serde_json::Value) -> ToolInput {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn relative_delays_are_stored_as_absolute_utc_instants() {
        let schedule = ScheduleSpec::from_input(
            &input(json!({
                "name": "Check issues",
                "prompt": "Check the issues",
                "delayMinutes": 120,
                "timeZone": "Asia/Shanghai"
            })),
            now(),
        )
        .unwrap();
        assert_eq!(schedule.at.unwrap().to_string(), "2026-10-07 11:00:00");
        assert_eq!(schedule.time_zone, "UTC");
        assert!(schedule.cron.is_empty());
    }

    #[test]
    fn a_relative_delay_does_not_shift_across_the_dst_fallback() {
        let now: DateTime<Utc> = "2026-11-01T05:00:00Z".parse().unwrap();
        let schedule = ScheduleSpec::from_input(
            &input(json!({
                "name": "n", "prompt": "p", "delayMinutes": 90,
                "timeZone": "America/New_York"
            })),
            now,
        )
        .unwrap();
        let parsed =
            crate::routines::parse_schedule(&schedule.cron, schedule.at, &schedule.time_zone)
                .unwrap();
        assert_eq!(
            crate::routines::next_after(&parsed, now)
                .unwrap()
                .to_rfc3339(),
            "2026-11-01T06:30:00+00:00"
        );
    }

    #[test]
    fn absolute_and_cron_schedules_pass_through() {
        let absolute = ScheduleSpec::from_input(
            &input(json!({
                "name": "n", "prompt": "p", "at": "2026-10-08T09:30:00",
                "timeZone": "UTC"
            })),
            now(),
        )
        .unwrap();
        assert_eq!(absolute.at.unwrap().to_string(), "2026-10-08 09:30:00");
        assert!(absolute.cron.is_empty());

        let recurring = ScheduleSpec::from_input(
            &input(json!({
                "name": "n", "prompt": "p", "cron": "0 9 * * *", "timeZone": "UTC"
            })),
            now(),
        )
        .unwrap();
        assert_eq!(recurring.cron, "0 9 * * *");
        assert!(recurring.at.is_none());
    }

    #[test]
    fn schedule_shapes_are_exclusive_and_positive() {
        for value in [
            json!({ "name": "n", "prompt": "p" }),
            json!({ "name": "n", "prompt": "p", "delayMinutes": 1, "cron": "* * * * *" }),
            json!({ "name": "n", "prompt": "p", "delayMinutes": 0 }),
            json!({ "name": "n", "prompt": "p", "delayMinutes": -1 }),
        ] {
            assert!(ScheduleSpec::from_input(&input(value), now()).is_err());
        }
    }
}
