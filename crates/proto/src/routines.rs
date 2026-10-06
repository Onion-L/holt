//! Routines (ADR-0042): a saved prompt the engine runs on a cron schedule
//! in a fresh Chat. The engine owns the schedule; these are the shapes the
//! Routines watch and mutations carry.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ChatConfig;

/// Run records a Routine keeps; older ones drop.
pub const ROUTINE_RUN_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub space_id: String,
    /// Sent verbatim as each run's first message.
    pub prompt: String,
    /// Five-field cron expression.
    pub cron: String,
    /// IANA time zone the cron is read in.
    pub time_zone: String,
    /// Model, reasoning, and Permission mode every run starts with.
    pub config: ChatConfig,
    pub checkout: RoutineCheckout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<RoutinePause>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Newest first, at most `ROUTINE_RUN_LIMIT`.
    #[serde(default)]
    pub runs: Vec<RoutineRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RoutineCheckout {
    #[default]
    MainCheckout,
    /// Each run gets its own session worktree (ADR-0038).
    NewWorktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutinePause {
    User,
    SpaceRemoved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRun {
    pub fired_at: DateTime<Utc>,
    pub outcome: RunOutcome,
    /// Failure reason, or "chat deleted".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The run's Chat; absent for skipped fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Fires a Catch-up run stands in for; 0 for a normal run.
    #[serde(default)]
    pub missed_fires: u32,
    /// Started by Run now.
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    Running,
    Waiting,
    Succeeded,
    Failed,
    Interrupted,
    Skipped,
}

impl RunOutcome {
    /// Running or waiting: a later fire is skipped meanwhile.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
}

/// The marker a Routine run Chat carries (`Chat::routine_run`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRunMarker {
    pub routine_id: String,
    /// The Routine's name when the run fired.
    pub routine_name: String,
    #[serde(default)]
    pub missed_fires: u32,
    #[serde(default)]
    pub manual: bool,
}

/// One row of the Routines watch: the stored Routine plus the engine's
/// derived next fire (absent while paused or when the cron never fires).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineView {
    #[serde(flatten)]
    pub routine: Routine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<DateTime<Utc>>,
}
