//! The engine's wall clock. Production reads the system time; tests inject a
//! manual clock through `EngineConfig::clock` and advance it, waking anything
//! sleeping on it.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

/// The longest a system-clock sleep goes without re-reading the wall clock.
/// Tokio timers run on the monotonic clock, which stops while the device
/// sleeps; re-checking bounds how late a deadline crossed during sleep is
/// noticed.
const SYSTEM_RECHECK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Clock(Arc<Inner>);

#[derive(Debug)]
enum Inner {
    System,
    /// The current time; `advance`/`set` publish through the watch so
    /// sleepers re-check their deadline.
    Manual(watch::Sender<DateTime<Utc>>),
}

impl Clock {
    pub fn system() -> Self {
        Self(Arc::new(Inner::System))
    }

    /// A clock frozen at `start` until a test moves it.
    pub fn manual(start: DateTime<Utc>) -> Self {
        Self(Arc::new(Inner::Manual(watch::channel(start).0)))
    }

    pub fn now(&self) -> DateTime<Utc> {
        match &*self.0 {
            Inner::System => Utc::now(),
            Inner::Manual(now) => *now.borrow(),
        }
    }

    /// Resolve once `now() >= deadline`.
    pub async fn sleep_until(&self, deadline: DateTime<Utc>) {
        match &*self.0 {
            Inner::System => loop {
                let Ok(remaining) = (deadline - Utc::now()).to_std() else {
                    return;
                };
                if remaining.is_zero() {
                    return;
                }
                tokio::time::sleep(remaining.min(SYSTEM_RECHECK)).await;
            },
            Inner::Manual(now) => {
                let mut rx = now.subscribe();
                // The sender lives as long as `self`, so this never errors.
                let _ = rx.wait_for(|now| *now >= deadline).await;
            }
        }
    }

    /// Move a manual clock forward. Panics on the system clock.
    pub fn advance(&self, by: chrono::Duration) {
        self.set(self.now() + by);
    }

    /// Jump a manual clock to `to`. Panics on the system clock.
    pub fn set(&self, to: DateTime<Utc>) {
        match &*self.0 {
            Inner::System => panic!("the system clock cannot be moved"),
            Inner::Manual(now) => {
                now.send_replace(to);
            }
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn advancing_a_manual_clock_wakes_sleepers() {
        let start = Utc::now();
        let clock = Clock::manual(start);
        let sleeper = tokio::spawn({
            let clock = clock.clone();
            async move {
                clock
                    .sleep_until(start + chrono::Duration::minutes(5))
                    .await
            }
        });
        clock.advance(chrono::Duration::minutes(4));
        tokio::task::yield_now().await;
        assert!(!sleeper.is_finished());
        clock.advance(chrono::Duration::minutes(1));
        tokio::time::timeout(Duration::from_secs(5), sleeper)
            .await
            .expect("sleeper woke")
            .unwrap();
        assert_eq!(clock.now(), start + chrono::Duration::minutes(5));
    }

    #[tokio::test]
    async fn a_past_deadline_resolves_immediately() {
        let clock = Clock::system();
        clock
            .sleep_until(Utc::now() - chrono::Duration::seconds(1))
            .await;
    }
}
