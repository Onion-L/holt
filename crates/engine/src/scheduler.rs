//! The Routine scheduler (ADR-0042): one long-lived task that sleeps until
//! the earliest next fire among active Routines, fires whatever is due, and
//! re-plans whenever the Routine list changes. Fires missed while Holt was
//! quit or the device slept are found the same way — at startup, or when the
//! wall clock has jumped past them on wake — and coalesce into one Catch-up
//! run.

use chrono::{DateTime, Utc};

use crate::EngineService;
use crate::routines::{Due, due_at, next_fire};
use crate::rpc::routines::Fire;

/// How long a fire whose `last_fired_at` could not be recorded waits before
/// it is retried, so a failing disk does not spin the loop.
const RETRY_AFTER: chrono::Duration = chrono::Duration::minutes(1);

impl EngineService {
    /// Start the scheduler on the current tokio runtime; a no-op outside
    /// one. It stops when the engine shuts down.
    pub(crate) fn spawn_scheduler(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let service = self.clone();
        handle.spawn(async move { service.schedule().await });
    }

    async fn schedule(self) {
        let mut changes = self.routines.subscribe();
        loop {
            changes.borrow_and_update();
            let now = self.clock.now();
            let mut wake: Option<DateTime<Utc>> = None;
            let mut plan = |at: DateTime<Utc>| wake = Some(wake.map_or(at, |wake| wake.min(at)));
            for routine in self.routines.list() {
                if let Some(due) = due_at(&routine, now) {
                    // A recorded fire changes the list, which re-plans.
                    if !self.fire_scheduled(&routine.id, due) {
                        plan(now + RETRY_AFTER);
                    }
                } else if let Some(next) = next_fire(&routine) {
                    plan(next);
                }
            }
            let sleep = async {
                match wake {
                    Some(at) => self.clock.sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = self.scheduler.cancelled() => return,
                changed = changes.changed() => if changed.is_err() { return },
                () = sleep => {}
            }
        }
    }

    /// Record `due.at` as the Routine's `last_fired_at`, then start its run.
    /// The fire is recorded first so a run that cannot start is not retried
    /// on every pass. Returns whether the fire was recorded.
    fn fire_scheduled(&self, id: &str, due: Due) -> bool {
        let recorded = self.routines.update(|routines| {
            if let Some(routine) = routines.iter_mut().find(|routine| routine.id == id) {
                routine.last_fired_at = Some(due.at);
            }
            Ok(())
        });
        if let Err(error) = recorded {
            tracing::warn!(routine = id, %error, "could not record a Routine fire");
            return false;
        }
        let fire = Fire {
            missed_fires: due.missed_fires,
            manual: false,
        };
        if let Err(error) = self.start_routine_run(id, fire) {
            tracing::warn!(routine = id, %error, "a scheduled Routine run did not start");
        }
        true
    }
}
