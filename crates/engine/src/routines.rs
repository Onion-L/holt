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
use holt_proto::{ROUTINE_RUN_LIMIT, Routine, RoutineRun, RoutineView};
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
        let routines: Vec<Routine> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                EngineError::Other(format!("could not read {}: {error}", path.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let (tx, _) = watch::channel(views(&routines));
        Ok(Self {
            path,
            state: Mutex::new(routines),
            tx,
        })
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
        self.persist(&next)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        *state = next;
        self.tx.send_replace(views(&state));
        Ok(out)
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

/// The latest scheduled time at or before `now`, if the Routine is due.
pub(crate) fn due_at(routine: &Routine, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut due = next_fire(routine).filter(|next| *next <= now)?;
    let (cron, zone) = parse_schedule(&routine.cron, &routine.time_zone).ok()?;
    while let Some(next) = next_after(&cron, zone, due).filter(|next| *next <= now) {
        due = next;
    }
    Some(due)
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
