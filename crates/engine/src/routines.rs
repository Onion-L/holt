//! The Routine store (ADR-0042): every Routine and its run records in one
//! JSON file under the data dir, written atomically. Mutations go through
//! `update`, which persists before the in-memory list moves and then
//! publishes the Routines watch.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use holt_proto::{ROUTINE_RUN_LIMIT, Routine, RoutineRun, RoutineView, RunOutcome};
use holt_rpc::RpcError;
use tokio::sync::watch;

use crate::EngineError;

const FILE_NAME: &str = "routines.json";

pub(crate) struct Routines {
    path: PathBuf,
    state: Mutex<Vec<Routine>>,
    tx: watch::Sender<serde_json::Value>,
}

impl Routines {
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let mut routines: Vec<Routine> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                EngineError::Other(format!("could not read {}: {error}", path.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        // Runs live when Holt quit never finish: they become interrupted.
        let mut interrupted = false;
        for run in routines
            .iter_mut()
            .flat_map(|routine| routine.runs.iter_mut())
        {
            if run.outcome.is_live() {
                run.outcome = RunOutcome::Interrupted;
                interrupted = true;
            }
        }
        let (tx, _) = watch::channel(views(&routines));
        let store = Self {
            path,
            state: Mutex::new(Vec::new()),
            tx,
        };
        if interrupted {
            store.persist(&routines)?;
        }
        *store.state.lock().unwrap_or_else(|e| e.into_inner()) = routines;
        Ok(store)
    }

    /// The current `Vec<RoutineView>` as served by `ListRoutines`.
    pub fn views(&self) -> serde_json::Value {
        self.tx.borrow().clone()
    }

    pub fn get(&self, id: &str) -> Option<Routine> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|routine| routine.id == id)
            .cloned()
    }

    /// Every Routine, as stored.
    pub fn list(&self) -> Vec<Routine> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<serde_json::Value> {
        self.tx.subscribe()
    }

    /// Mutate a copy of the list; on `Ok` persist it, swap it in, and
    /// publish. An `Err` from `f` or a failed write leaves everything as
    /// it was.
    pub fn update<R>(
        &self,
        f: impl FnOnce(&mut Vec<Routine>) -> Result<R, RpcError>,
    ) -> Result<R, RpcError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let out = f(&mut next)?;
        self.commit(&mut state, next)?;
        Ok(out)
    }

    /// Mutate the run record of the run Chat `chat_id`, if any Routine has
    /// one. `f` returns whether it changed the record; nothing is written
    /// otherwise.
    pub fn update_run(
        &self,
        chat_id: &str,
        f: impl FnOnce(&mut RoutineRun) -> bool,
    ) -> Result<(), RpcError> {
        let is_run = |run: &RoutineRun| run.chat_id.as_deref() == Some(chat_id);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.iter().any(|routine| routine.runs.iter().any(is_run)) {
            return Ok(());
        }
        let mut next = state.clone();
        let changed = next
            .iter_mut()
            .flat_map(|routine| routine.runs.iter_mut())
            .find(|run| is_run(run))
            .is_some_and(f);
        if changed {
            self.commit(&mut state, next)?;
        }
        Ok(())
    }

    fn commit(&self, state: &mut Vec<Routine>, next: Vec<Routine>) -> Result<(), RpcError> {
        self.persist(&next)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        *state = next;
        self.tx.send_replace(views(state));
        Ok(())
    }

    fn persist(&self, routines: &[Routine]) -> Result<(), EngineError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("routines path has no parent".into()))?;
        let temp = parent.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(routines)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, &self.path));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(EngineError::Io)
    }
}

/// Prepend a run record, keeping the newest `ROUTINE_RUN_LIMIT`.
pub(crate) fn push_run(routine: &mut Routine, run: RoutineRun) {
    routine.runs.insert(0, run);
    routine.runs.truncate(ROUTINE_RUN_LIMIT);
}

/// The local IANA time zone, falling back to UTC when the system cannot
/// name one.
pub(crate) fn local_time_zone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string())
}

/// Parse a Routine's schedule: a cron expression read in an IANA zone.
pub(crate) fn parse_schedule(cron: &str, time_zone: &str) -> Result<(Cron, Tz), RpcError> {
    let cron = Cron::from_str(cron.trim())
        .map_err(|error| RpcError::BadParams(format!("invalid cron: {error}")))?;
    let zone = Tz::from_str(time_zone.trim())
        .map_err(|_| RpcError::BadParams(format!("unknown time zone: {time_zone}")))?;
    Ok((cron, zone))
}

/// The first scheduled fire strictly after `after`. A local time skipped by
/// a DST jump fires at the next valid time; a repeated one fires once.
pub(crate) fn next_after(cron: &Cron, zone: Tz, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    cron.find_next_occurrence(&after.with_timezone(&zone), false)
        .ok()
        .map(|next| next.with_timezone(&Utc))
}

/// A Routine's next fire: the first scheduled time after it last fired (or
/// was created). `None` while paused or when the schedule cannot be read.
/// It can lie in the past until the scheduler catches up.
pub(crate) fn next_fire(routine: &Routine) -> Option<DateTime<Utc>> {
    if routine.paused.is_some() {
        return None;
    }
    let (cron, zone) = parse_schedule(&routine.cron, &routine.time_zone).ok()?;
    let after = routine
        .last_fired_at
        .map_or(routine.created_at, |last| last.max(routine.created_at));
    next_after(&cron, zone, after)
}

/// A fire later than this behind its scheduled time was missed (Holt was
/// quit or the device slept) and runs as a Catch-up run.
const LATE_AFTER: chrono::Duration = chrono::Duration::minutes(2);

/// What a due Routine fires now.
#[derive(Debug, PartialEq)]
pub(crate) struct Due {
    /// The latest scheduled time at or before now; the new `last_fired_at`.
    pub at: DateTime<Utc>,
    /// Fires the Catch-up run stands in for; 0 for an on-time fire.
    pub missed_fires: u32,
}

/// Whether the Routine is due at `now`, and how many fires passed since it
/// last fired. One on-time fire is a normal fire; anything else coalesces
/// into one Catch-up run. Never while paused.
pub(crate) fn due_at(routine: &Routine, now: DateTime<Utc>) -> Option<Due> {
    let mut at = next_fire(routine).filter(|next| *next <= now)?;
    let (cron, zone) = parse_schedule(&routine.cron, &routine.time_zone).ok()?;
    let mut fires: u32 = 1;
    while let Some(next) = next_after(&cron, zone, at).filter(|next| *next <= now) {
        at = next;
        fires = fires.saturating_add(1);
    }
    let on_time = fires == 1 && now - at <= LATE_AFTER;
    Some(Due {
        at,
        missed_fires: if on_time { 0 } else { fires },
    })
}

fn views(routines: &[Routine]) -> serde_json::Value {
    let views: Vec<RoutineView> = routines
        .iter()
        .map(|routine| RoutineView {
            routine: routine.clone(),
            next_fire_at: next_fire(routine),
        })
        .collect();
    serde_json::to_value(views).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(cron: &str) -> Routine {
        serde_json::from_value(serde_json::json!({
            "id": "r", "name": "r", "spaceId": "s", "prompt": "p", "cron": cron,
            "timeZone": "UTC",
            "config": { "provider": "openai", "model": "m", "reasoning": null,
                        "permissionMode": "auto-review" },
            "checkout": "main-checkout", "createdAt": "2026-10-07T09:00:00Z",
        }))
        .unwrap()
    }

    fn at(time: &str) -> DateTime<Utc> {
        time.parse().unwrap()
    }

    #[test]
    fn due_at_tells_on_time_fires_from_missed_ones() {
        let hourly = routine("0 * * * *");
        let due = |now| due_at(&hourly, at(now)).map(|due| (due.at, due.missed_fires));
        assert_eq!(due("2026-10-07T09:59:00Z"), None);
        assert_eq!(
            due("2026-10-07T10:00:30Z"),
            Some((at("2026-10-07T10:00:00Z"), 0))
        );
        // One fire, but well past its time: Holt was not running.
        assert_eq!(
            due("2026-10-07T10:30:00Z"),
            Some((at("2026-10-07T10:00:00Z"), 1))
        );
        // Every fire since the anchor, on time or not, coalesces.
        assert_eq!(
            due("2026-10-07T13:00:00Z"),
            Some((at("2026-10-07T13:00:00Z"), 4))
        );
        let mut paused = hourly.clone();
        paused.paused = Some(holt_proto::RoutinePause::User);
        assert_eq!(due_at(&paused, at("2026-10-07T13:00:00Z")), None);
    }

    #[test]
    fn push_run_keeps_the_newest_records() {
        let mut routine = routine("* * * * *");
        let start = routine.created_at;
        for minute in 0..ROUTINE_RUN_LIMIT as i64 + 5 {
            push_run(
                &mut routine,
                RoutineRun {
                    fired_at: start + chrono::Duration::minutes(minute),
                    outcome: RunOutcome::Succeeded,
                    note: None,
                    chat_id: None,
                    missed_fires: 0,
                    manual: false,
                },
            );
        }
        assert_eq!(routine.runs.len(), ROUTINE_RUN_LIMIT);
        assert_eq!(
            routine.runs[0].fired_at,
            start + chrono::Duration::minutes(ROUTINE_RUN_LIMIT as i64 + 4)
        );
        assert_eq!(
            routine.runs[ROUTINE_RUN_LIMIT - 1].fired_at,
            start + chrono::Duration::minutes(5)
        );
    }
}
