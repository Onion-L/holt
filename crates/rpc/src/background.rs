//! Typed background-process control parameters. Output events reuse
//! `holt_proto::TerminalEvent` — the wire shape (base64 data, monotonic
//! seqs, `Gap` on eviction, final `Exit`) is the same bounded-log contract
//! terminals serve.

use serde::{Deserialize, Serialize};

/// One row of `ListBackgroundProcesses` / `WatchBackgroundProcesses`.
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
    use super::BackgroundProcessStatus;

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
        assert!(parsed.exit_code.is_none() && parsed.exited_at.is_none());
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
    }
}
