//! The integration checks surface: one run of the repository's declared
//! checks against a worktree's in-memory merge with the trunk
//! (`.holt/checks.toml` on the trunk). Never a merge gate — the results
//! inform the merge bar, nothing more.

use serde::{Deserialize, Serialize};

/// One check's row inside a [`CheckRun`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRow {
    pub name: String,
    /// `pending` | `running` | `passed` | `failed` | `timeout` | `stopped` | `error`
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The tail of the combined output (the full log lives in memory,
    /// streamed by `SubscribeCheckLog`).
    #[serde(default)]
    pub log_tail: String,
}

/// One run's state — every `WatchChecks` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    pub run_id: String,
    /// The trunk the run merged against (staleness re-reads this ref at
    /// watch time).
    #[serde(default)]
    pub trunk: String,
    /// The merge the run executed against (the squash of these two heads).
    pub trunk_head: String,
    pub branch_head: String,
    /// Either head moved after the run started — the results describe a
    /// merge that no longer exists.
    #[serde(default)]
    pub stale: bool,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub checks: Vec<CheckRow>,
    /// Why the run could not start or stopped early (a conflicting merge
    /// preview, an invalid checks.toml).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CheckRun {
    pub fn finished(&self) -> bool {
        self.finished_at.is_some()
    }
}
