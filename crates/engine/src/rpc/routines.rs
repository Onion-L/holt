//! The Routines surface (ADR-0042): list, create, update, schedule preview,
//! delete, pause, Run now, and the Routines watch. Every fire goes through `start_routine_run`.

use chrono::{DateTime, NaiveDateTime, Utc};
use holt_proto::{
    Chat, ChatConfig, Routine, RoutineCheckout, RoutinePause, RoutineRun, RoutineRunMarker,
    RunOutcome, TitleSource, WorktreeSpec,
};
use holt_rpc::{RpcError, RpcReply};
use serde::Deserialize;

use super::required_string;
use crate::EngineService;
use crate::routines::{Schedule, local_time_zone, next_after, parse_schedule, push_run};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutineParams {
    name: String,
    space_id: String,
    prompt: String,
    #[serde(default)]
    cron: String,
    #[serde(default)]
    at: Option<NaiveDateTime>,
    #[serde(default)]
    time_zone: Option<String>,
    config: ChatConfig,
    #[serde(default)]
    checkout: RoutineCheckout,
}

/// A Routine's configuration, validated: everything but its identity,
/// pause state, and run history.
struct RoutineConfig {
    name: String,
    space_id: String,
    prompt: String,
    cron: String,
    at: Option<NaiveDateTime>,
    time_zone: String,
    config: ChatConfig,
    checkout: RoutineCheckout,
}

/// How many upcoming fires the schedule preview lists.
const PREVIEW_FIRES: usize = 3;

/// How a run came about; recorded on the run and on its Chat's marker.
pub(crate) struct Fire {
    pub missed_fires: u32,
    pub manual: bool,
    /// A scheduled fire's time, recorded as `last_fired_at` with the run.
    pub scheduled_at: Option<DateTime<Utc>>,
}

impl EngineService {
    pub(super) fn create_routine(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let config = self.routine_config(params)?;
        let routine = Routine {
            id: uuid::Uuid::new_v4().to_string(),
            name: config.name,
            space_id: config.space_id,
            prompt: config.prompt,
            cron: config.cron,
            at: config.at,
            time_zone: config.time_zone,
            config: config.config,
            checkout: config.checkout,
            paused: None,
            created_at: self.clock.now(),
            last_fired_at: None,
            runs: Vec::new(),
        };
        let reply = routine.clone();
        self.routines.update(|routines| {
            routines.push(routine);
            Ok(())
        })?;
        RpcReply::value(&reply)
    }

    /// Replace a Routine's configuration. Run records and pause state stay;
    /// a changed schedule is re-planned from now, so the new cron never
    /// makes up fires that passed before the edit.
    pub(super) fn update_routine(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "routineId")?.to_string();
        let config = self.routine_config(params)?;
        let now = self.clock.now();
        let reply = self.routines.update(|routines| {
            let routine = routines
                .iter_mut()
                .find(|routine| routine.id == id)
                .ok_or_else(|| RpcError::BadParams("unknown routine".into()))?;
            // Pointing a Routine whose Space was removed at an existing
            // one resumes it, from now like any resume.
            let resumed = routine.paused == Some(RoutinePause::SpaceRemoved);
            if resumed {
                routine.paused = None;
            }
            if resumed
                || routine.cron != config.cron
                || routine.at != config.at
                || routine.time_zone != config.time_zone
            {
                routine.last_fired_at =
                    Some(routine.last_fired_at.map_or(now, |last| last.max(now)));
            }
            routine.name = config.name;
            routine.space_id = config.space_id;
            routine.prompt = config.prompt;
            routine.cron = config.cron;
            routine.at = config.at;
            routine.time_zone = config.time_zone;
            routine.config = config.config;
            routine.checkout = config.checkout;
            Ok(routine.clone())
        })?;
        RpcReply::value(&reply)
    }

    /// Params `{cron | at, timeZone?}` → `{timeZone, fires}`: the zone the
    /// schedule is read in and its next fires from now (a one-time `at`
    /// has just the one), or `BadParams`
    /// naming what is wrong.
    pub(super) fn preview_routine_schedule(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let cron = params
            .get("cron")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let at = match params.get("at") {
            None | Some(serde_json::Value::Null) => None,
            Some(at) => Some(
                serde_json::from_value::<NaiveDateTime>(at.clone())
                    .map_err(|error| RpcError::BadParams(format!("invalid time: {error}")))?,
            ),
        };
        let time_zone = time_zone_or_local(
            params
                .get("timeZone")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        );
        let schedule = parse_schedule(cron, at, &time_zone)?;
        let now = self.clock.now();
        if let Schedule::Once(at, _) = schedule
            && at <= now
        {
            return Err(RpcError::BadParams("that time has passed".into()));
        }
        // The schedule's own zone: the fires travel as wall-clock times in
        // it, so the form never does zone math (chrono-tz stays engine-only).
        let zone = match &schedule {
            Schedule::Cron(_, zone) | Schedule::Once(_, zone) => *zone,
        };
        let mut fires = Vec::with_capacity(PREVIEW_FIRES);
        let mut after = now;
        while fires.len() < PREVIEW_FIRES {
            let Some(next) = next_after(&schedule, after) else {
                break;
            };
            fires.push(next.with_timezone(&zone));
            after = next;
        }
        RpcReply::value(&serde_json::json!({ "timeZone": time_zone, "fires": fires }))
    }

    fn routine_config(&self, params: serde_json::Value) -> Result<RoutineConfig, RpcError> {
        let params: RoutineParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let name = params.name.trim();
        if name.is_empty() {
            return Err(RpcError::BadParams("name must not be empty".into()));
        }
        if params.prompt.trim().is_empty() {
            return Err(RpcError::BadParams("prompt must not be empty".into()));
        }
        let time_zone = time_zone_or_local(params.time_zone);
        if let Schedule::Once(at, _) = parse_schedule(&params.cron, params.at, &time_zone)?
            && at <= self.clock.now()
        {
            return Err(RpcError::BadParams("that time has passed".into()));
        }
        if !self.space_exists(&params.space_id) {
            return Err(RpcError::BadParams("unknown space".into()));
        }
        Ok(RoutineConfig {
            name: name.to_string(),
            space_id: params.space_id,
            prompt: params.prompt,
            cron: params.cron.trim().to_string(),
            at: params.at,
            time_zone,
            config: params.config,
            checkout: params.checkout,
        })
    }

    pub(super) fn delete_routine(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "routineId")?;
        // Run chats keep their marker and become ordinary Chats.
        self.routines.update(|routines| {
            routines.retain(|routine| routine.id != id);
            Ok(())
        })?;
        RpcReply::value(&serde_json::json!({}))
    }

    /// Pause (reason "user") or resume a Routine. Resuming moves the
    /// schedule's anchor to now, so fires passed while paused are not made
    /// up and the next fire is the first one in the future. A Routine
    /// paused because its Space was removed keeps that reason; only an edit
    /// to another Space resumes it.
    pub(super) fn set_routine_paused(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "routineId")?;
        let paused = params
            .get("paused")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| RpcError::BadParams("paused must be a boolean".into()))?;
        let now = self.clock.now();
        self.routines.update(|routines| {
            let routine = routines
                .iter_mut()
                .find(|routine| routine.id == id)
                .ok_or_else(|| RpcError::BadParams("unknown routine".into()))?;
            if routine.paused == Some(RoutinePause::SpaceRemoved) {
                if paused {
                    return Ok(());
                }
                return Err(RpcError::BadParams(
                    "its project was removed; edit it to pick another".into(),
                ));
            }
            if paused {
                routine.paused = Some(RoutinePause::User);
            } else if routine.paused.take().is_some() {
                routine.last_fired_at =
                    Some(routine.last_fired_at.map_or(now, |last| last.max(now)));
            }
            Ok(())
        })?;
        RpcReply::value(&serde_json::json!({}))
    }

    /// → `{chatId}`, or `{}` when the fire was skipped.
    pub(super) async fn run_routine_now(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "routineId")?;
        let chat_id = self
            .start_routine_run(
                id,
                Fire {
                    missed_fires: 0,
                    manual: true,
                    scheduled_at: None,
                },
            )
            .await?;
        RpcReply::value(&match chat_id {
            Some(chat_id) => serde_json::json!({ "chatId": chat_id }),
            None => serde_json::json!({}),
        })
    }

    /// Fire a Routine: create its run Chat (the Routine's name as a
    /// user-owned title, its model and Permission mode, the run marker),
    /// enqueue the prompt verbatim as the first message, and record the
    /// run. Returns the new Chat's id, or `None` when a run is still live
    /// and the fire is recorded as skipped instead, or when a pause raced
    /// the plan. A removed Space pauses
    /// the Routine and records nothing; an unavailable model records a
    /// failed run and leaves the Routine active — both reply the reason.
    pub(crate) async fn start_routine_run(
        &self,
        id: &str,
        fire: Fire,
    ) -> Result<Option<String>, RpcError> {
        let routine = self
            .routines
            .get(id)
            .ok_or_else(|| RpcError::BadParams("unknown routine".into()))?;
        if self.routine_space(&routine.space_id).is_none() {
            self.pause_routines_in_space(&routine.space_id);
            return Err(RpcError::Failed(
                "its project was removed; edit it to pick another".into(),
            ));
        }
        let now = self.clock.now();
        let chat_id = uuid::Uuid::new_v4().to_string();
        // Claim the fire in one write: the skip check, the run record (live
        // from here, so a racing fire is skipped and a fast first Turn finds
        // it to settle), and a scheduled fire's `last_fired_at`. A quit
        // before the Chat exists leaves an interrupted run, not a lost fire.
        let claimed = self.routines.update(|routines| {
            let routine = routines
                .iter_mut()
                .find(|routine| routine.id == id)
                .ok_or_else(|| RpcError::BadParams("unknown routine".into()))?;
            // A pause that raced the plan stands the fire down before it
            // claims anything; Run now (manual) still goes — the card offers
            // it on a user-paused Routine.
            if routine.paused.is_some() && !fire.manual {
                return Ok(None);
            }
            if let Some(at) = fire.scheduled_at {
                routine.last_fired_at = Some(at);
            }
            // Skipped records land on top of the live run, so look past them.
            let live = routine.runs.iter().any(|run| run.outcome.is_live());
            let run = RoutineRun {
                fired_at: now,
                outcome: if live {
                    RunOutcome::Skipped
                } else {
                    RunOutcome::Running
                },
                note: None,
                chat_id: (!live).then(|| chat_id.clone()),
                missed_fires: fire.missed_fires,
                manual: fire.manual,
            };
            push_run(routine, run);
            Ok((!live).then(|| routine.clone()))
        })?;
        let Some(routine) = claimed else {
            return Ok(None);
        };
        let Some(space) = self.routine_space(&routine.space_id) else {
            let reason = "its project was removed; edit it to pick another";
            self.fail_routine_run(&chat_id, reason, false);
            return Err(RpcError::Failed(reason.into()));
        };
        if let Some(reason) = self.model_unavailable(&routine.config).await {
            self.fail_routine_run(&chat_id, &reason, false);
            return Err(RpcError::Failed(reason));
        }
        self.runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Chat {
                id: chat_id.clone(),
                device_id: space.device_id.clone(),
                title: Some(routine.name.clone()),
                title_source: TitleSource::UserManual,
                title_task_started: false,
                archived: false,
                pinned: false,
                cwd: Some(space.path.clone()),
                branch: None,
                checkout_id: space.checkout_id.clone(),
                source_context: None,
                config: Some(routine.config.clone()),
                last_message_preview: None,
                last_message_at: None,
                created_at: now,
                space_id: Some(space.id.clone()),
                last_seen_at: None,
                room_gen: None,
                compact_before_next_turn: false,
                plan_mode: None,
                provider_mode: false,
                // The session worktree (ADR-0038) is cut from the repo's
                // current checkout, like a composer send with no picked
                // base.
                worktree: (routine.checkout == RoutineCheckout::NewWorktree).then(|| {
                    WorktreeSpec {
                        repo_path: space.path.clone(),
                        base: "HEAD".into(),
                    }
                }),
                routine_run: Some(RoutineRunMarker {
                    routine_id: routine.id.clone(),
                    routine_name: routine.name.clone(),
                    missed_fires: fire.missed_fires,
                    manual: fire.manual,
                }),
            });
        let started = self
            .runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))
            .and_then(|()| {
                let chat = self.runtime.chat(&chat_id);
                self.runtime.publish_chats();
                let request =
                    Self::queued_run_request(&routine.config, &routine.prompt, space.path);
                self.enqueue_run(chat, request, uuid::Uuid::new_v4().to_string())
            });
        if let Err(error) = started {
            self.fail_routine_run(&chat_id, &error.to_string(), true);
            return Err(error);
        }
        Ok(Some(chat_id))
    }

    /// A claimed run could not start: record it failed with the reason.
    /// `keep_chat` leaves the record pointing at its Chat, when one exists.
    fn fail_routine_run(&self, chat_id: &str, reason: &str, keep_chat: bool) {
        let result = self.routines.update_run(chat_id, |run| {
            run.outcome = RunOutcome::Failed;
            run.note = Some(reason.to_string());
            if !keep_chat {
                run.chat_id = None;
            }
            true
        });
        if let Err(error) = result {
            tracing::warn!(chat_id, %error, "could not record a Routine run failure");
        }
    }

    fn routine_space(&self, space_id: &str) -> Option<holt_proto::Space> {
        self.spaces
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|space| space.id == space_id)
            .cloned()
    }

    /// A run Chat's Turn settled: the first one decides the run's outcome.
    /// Later Turns find the record no longer live and leave it alone.
    pub(crate) fn settle_routine_run(&self, chat_id: &str, outcome: RunOutcome) {
        let result = self.routines.update_run(chat_id, |run| {
            if !run.outcome.is_live() {
                return false;
            }
            run.outcome = outcome;
            true
        });
        if let Err(error) = result {
            tracing::warn!(chat_id, %error, "could not record a Routine run outcome");
        }
    }

    /// A run Chat was deleted: its record stays, no longer openable.
    pub(crate) fn forget_routine_run_chat(&self, chat_id: &str) {
        let result = self.routines.update_run(chat_id, |run| {
            run.chat_id = None;
            run.note = Some("chat deleted".into());
            if run.outcome.is_live() {
                run.outcome = RunOutcome::Interrupted;
            }
            true
        });
        if let Err(error) = result {
            tracing::warn!(chat_id, %error, "could not mark a Routine run's Chat deleted");
        }
    }

    /// Pause every Routine in a removed Space (reason "Space removed"). No
    /// run is recorded.
    pub(crate) fn pause_routines_in_space(&self, space_id: &str) {
        let result = self.routines.update(|routines| {
            for routine in routines
                .iter_mut()
                .filter(|routine| routine.space_id == space_id)
            {
                routine.paused = Some(RoutinePause::SpaceRemoved);
            }
            Ok(())
        });
        if let Err(error) = result {
            tracing::warn!(space_id, %error, "could not pause the Routines of a removed Space");
        }
    }

    /// Why a run on `config` could not start: its model is gone from the
    /// catalog or its provider has no key. Same wording as the queue's.
    async fn model_unavailable(&self, config: &ChatConfig) -> Option<String> {
        let provider = config.provider.as_str();
        if let Err(error) = self.providers.resolve_model(provider, &config.model) {
            return Some(error);
        }
        if self
            .providers
            .credentials
            .reveal_key(provider)
            .await
            .is_none()
        {
            return Some(format!("provider {provider} is not configured"));
        }
        None
    }

    fn space_exists(&self, space_id: &str) -> bool {
        self.routine_space(space_id).is_some()
    }
}

/// A requested time zone, or the device's when none is given.
fn time_zone_or_local(time_zone: Option<String>) -> String {
    time_zone
        .filter(|zone| !zone.trim().is_empty())
        .map_or_else(local_time_zone, |zone| zone.trim().to_string())
}
