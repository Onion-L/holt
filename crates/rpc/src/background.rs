//! Typed background-process control parameters. Output events reuse
//! `holt_proto::TerminalEvent` — the wire shape (base64 data, monotonic
//! seqs, `Gap` on eviction, final `Exit`) is the same bounded-log contract
//! terminals serve.

use serde::{Deserialize, Serialize};

/// What one row of the background-process list is: an agent-spawned
/// command, or a live background Workflow run (issue #77) — the popover
/// shows both, and a workflow row's stop goes to `StopWorkflow`, not
/// `KillBackgroundProcess`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundProcessKind {
    #[default]
    Process,
    Workflow,
}

/// One row of `ListBackgroundProcesses` / `WatchBackgroundProcesses`.
/// `kind` separates the two row shapes: a `Process` carries pid/exit
/// fields; a `Workflow` row (one live run — it leaves the list when the
/// run settles, its transcript chip carries the outcome) carries
/// `agents`/`tokens` progress and stops through `StopWorkflow` with
/// `processId` as the run's `callId`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundProcessStatus {
    pub process_id: String,
    pub chat_id: String,
    pub command: String,
    pub cwd: String,
    pub pid: Option<u32>,
    /// Whether the process's whole group is still alive. An exited process
    /// stays listed with its `exitCode` until the user clears it.
    pub running: bool,
    /// Unix epoch milliseconds.
    pub started_at: u64,
    pub exit_code: Option<i32>,
    /// Unix epoch milliseconds.
    pub exited_at: Option<u64>,
    #[serde(default)]
    pub kind: BackgroundProcessKind,
    /// Workflow rows: agents the run has started so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<usize>,
    /// Workflow rows: gross tokens the run's children have spent so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeBackgroundProcess {
    pub process_id: String,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillBackgroundProcess {
    pub process_id: String,
}

/// Remove one exited entry from the list; a running process refuses.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveBackgroundProcess {
    pub process_id: String,
}

/// Remove every exited entry of one chat.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearExitedBackgroundProcesses {
    pub chat_id: String,
}

#[cfg(test)]
mod tests {
    use super::{BackgroundProcessKind, BackgroundProcessStatus};

    #[test]
    fn status_parses_tolerantly_without_optional_fields() {
        let parsed: BackgroundProcessStatus = serde_json::from_value(serde_json::json!({
            "processId": "p1",
            "chatId": "c1",
            "command": "npm run dev",
            "cwd": "/tmp",
            "pid": 42,
            "running": true,
            "startedAt": 1000u64
        }))
        .unwrap();
        // No `kind` on the wire: a process row, the pre-workflow shape.
        assert_eq!(parsed.kind, BackgroundProcessKind::Process);
        assert!(parsed.exit_code.is_none() && parsed.exited_at.is_none());
        assert!(parsed.agents.is_none() && parsed.tokens.is_none());
        let round: serde_json::Value = serde_json::to_value(
            serde_json::from_value::<BackgroundProcessStatus>(serde_json::json!({
                "processId": "p1", "chatId": "c1", "command": "x", "cwd": "/t",
                "running": false, "startedAt": 1u64, "exitCode": 3, "exitedAt": 9u64
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(round["exitCode"], 3);
        assert_eq!(round["processId"], "p1");
        assert_eq!(round["kind"], "process");
        // Absent progress fields stay off a process row's wire form.
        assert!(round.get("agents").is_none() && round.get("tokens").is_none());
    }

    #[test]
    fn a_workflow_row_round_trips_its_progress() {
        let row: serde_json::Value = serde_json::to_value(BackgroundProcessStatus {
            process_id: "wf-1".into(),
            chat_id: "c1".into(),
            command: "audit".into(),
            cwd: String::new(),
            pid: None,
            running: true,
            started_at: 5u64,
            exit_code: None,
            exited_at: None,
            kind: BackgroundProcessKind::Workflow,
            agents: Some(3),
            tokens: Some(3400),
        })
        .unwrap();
        assert_eq!(row["kind"], "workflow");
        assert_eq!(row["agents"], 3);
        assert_eq!(row["tokens"], 3400);
        let parsed: BackgroundProcessStatus = serde_json::from_value(row).unwrap();
        assert_eq!(parsed.kind, BackgroundProcessKind::Workflow);
        assert_eq!(parsed.agents, Some(3));
    }
}
