//! Typed terminal control parameters. Output events live in holt-proto.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminal {
    pub chat_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalId {
    pub terminal_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTerminal {
    pub terminal_id: String,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTerminal {
    pub terminal_id: String,
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeTerminal {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStatus {
    pub terminal_id: String,
    pub chat_id: String,
    /// Whether the terminal's shell/PTY is still alive.
    pub running: bool,
    /// Whether a process other than the terminal's shell is alive in its
    /// session. An idle shell is not considered a running job.
    pub has_running_jobs: bool,
}
