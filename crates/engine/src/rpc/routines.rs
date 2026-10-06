//! The Routines surface (ADR-0042): list, create, update, schedule preview,
//! delete, pause, Run now, and the Routines watch. Every fire goes through `start_routine_run`.

use holt_proto::{
    Chat, ChatConfig, Routine, RoutineCheckout, RoutinePause, RoutineRun, RoutineRunMarker,
    RunOutcome, TitleSource, WorktreeSpec,
};
use holt_rpc::{RpcError, RpcReply};
use serde::Deserialize;

use super::required_string;
use crate::EngineService;
use crate::routines::{local_time_zone, next_after, parse_schedule, push_run};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutineParams {
    name: String,
    space_id: String,
    prompt: String,
    cron: String,
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
            if routine.cron != config.cron || routine.time_zone != config.time_zone {
                routine.last_fired_at =
                    Some(routine.last_fired_at.map_or(now, |last| last.max(now)));
            }
            routine.name = config.name;
            routine.space_id = config.space_id;
            routine.prompt = config.prompt;
            routine.cron = config.cron;
            routine.time_zone = config.time_zone;
            routine.config = config.config;
            routine.checkout = config.checkout;
            Ok(routine.clone())
        })?;
        RpcReply::value(&reply)
    }

    /// Params `{cron, timeZone?}` → `{timeZone, fires}`: the zone the
    /// schedule is read in and its next fires from now, or `BadParams`
    /// naming what is wrong.
    pub(super) fn preview_routine_schedule(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let cron = params
            .get("cron")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if cron.trim().is_empty() {
            return Err(RpcError::BadParams("cron must not be empty".into()));
        }
        let time_zone = time_zone_or_local(
            params
                .get("timeZone")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        );
        let (cron, zone) = parse_schedule(cron, &time_zone)?;
        let mut fires = Vec::with_capacity(PREVIEW_FIRES);
        let mut after = self.clock.now();
        while fires.len() < PREVIEW_FIRES {
            let Some(next) = next_after(&cron, zone, after) else {
                break;
            };
            fires.push(next);
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
        if params.cron.trim().is_empty() {
            return Err(RpcError::BadParams("cron must not be empty".into()));
        }
        let time_zone = time_zone_or_local(params.time_zone);
        parse_schedule(&params.cron, &time_zone)?;
        if !self.space_exists(&params.space_id) {
            return Err(RpcError::BadParams("unknown space".into()));
        }
        Ok(RoutineConfig {
            name: name.to_string(),
            space_id: params.space_id,
            prompt: params.prompt,
            cron: params.cron.trim().to_string(),
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
    /// up and the next fire is the first one in the future.
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
    pub(super) fn run_routine_now(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let id = required_string(&params, "routineId")?;
        let chat_id = self.start_routine_run(
            id,
            Fire {
                missed_fires: 0,
                manual: true,
            },
        )?;
        RpcReply::value(&match chat_id {
            Some(chat_id) => serde_json::json!({ "chatId": chat_id }),
            None => serde_json::json!({}),
        })
    }

    /// Fire a Routine: create its run Chat (the Routine's name as a
    /// user-owned title, its model and Permission mode, the run marker),
    /// enqueue the prompt verbatim as the first message, and record the
    /// run. Returns the new Chat's id, or `None` when a run is still live
    /// and the fire is recorded as skipped instead.
    pub(crate) fn start_routine_run(
        &self,
        id: &str,
        fire: Fire,
    ) -> Result<Option<String>, RpcError> {
        let routine = self
            .routines
            .get(id)
            .ok_or_else(|| RpcError::BadParams("unknown routine".into()))?;
        let now = self.clock.now();
        // Skipped records land on top of the live run, so look past them.
        if routine.runs.iter().any(|run| run.outcome.is_live()) {
            let run = RoutineRun {
                fired_at: now,
                outcome: RunOutcome::Skipped,
                note: None,
                chat_id: None,
                missed_fires: fire.missed_fires,
                manual: fire.manual,
            };
            self.routines.update(|routines| {
                if let Some(routine) = routines.iter_mut().find(|routine| routine.id == id) {
                    push_run(routine, run);
                }
                Ok(())
            })?;
            return Ok(None);
        }
        let space = self
            .spaces
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|space| space.id == routine.space_id)
            .cloned()
            .ok_or_else(|| RpcError::Failed("the Routine's Space was removed".into()))?;
        let chat_id = uuid::Uuid::new_v4().to_string();
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
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let chat = self.runtime.chat(&chat_id);
        self.runtime.publish_chats();
        let request = Self::queued_run_request(&routine.config, &routine.prompt, space.path);
        self.enqueue_run(chat, request, uuid::Uuid::new_v4().to_string())?;
        let run = RoutineRun {
            fired_at: now,
            outcome: RunOutcome::Running,
            note: None,
            chat_id: Some(chat_id.clone()),
            missed_fires: fire.missed_fires,
            manual: fire.manual,
        };
        self.routines.update(|routines| {
            if let Some(routine) = routines.iter_mut().find(|routine| routine.id == id) {
                push_run(routine, run);
            }
            Ok(())
        })?;
        Ok(Some(chat_id))
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

    fn space_exists(&self, space_id: &str) -> bool {
        self.spaces
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|space| space.id == space_id)
    }
}

/// A requested time zone, or the device's when none is given.
fn time_zone_or_local(time_zone: Option<String>) -> String {
    time_zone
        .filter(|zone| !zone.trim().is_empty())
        .map_or_else(local_time_zone, |zone| zone.trim().to_string())
}
