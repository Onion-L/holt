//! Chat-owned background processes over the RPC contract
//! (ADR-background-processes-are-chat-owned).

use holt_rpc::background::BackgroundProcessStatus;
use holt_rpc::{RpcError, RpcReply};

use crate::EngineService;
use crate::agent::AgentRuntime;

/// The popover's rows: the process registry's list plus the live workflow
/// runs (issue #77), oldest first. Workflow rows stop through
/// `StopWorkflow`; the agent's own process tools keep seeing processes
/// only.
fn merged_rows(
    background: &crate::background::BackgroundProcesses,
    runtime: &AgentRuntime,
) -> Vec<BackgroundProcessStatus> {
    let mut rows = background.list();
    rows.extend(runtime.workflows.list());
    rows.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.process_id.cmp(&b.process_id))
    });
    rows
}

impl EngineService {
    pub(super) fn background_rows(&self) -> Vec<BackgroundProcessStatus> {
        merged_rows(&self.background, &self.runtime)
    }

    /// The live list stream: the current snapshot first, then a fresh
    /// snapshot on every structural change — a process spawning, exiting,
    /// or being removed, or a workflow run launching, progressing, or
    /// settling. Either registry's bump wakes it; both receivers ride the
    /// stream's state so no change between snapshots is missed.
    pub(super) fn watch_background_processes(&self) -> RpcReply {
        let background = self.background.clone();
        let runtime = self.runtime.clone();
        let processes = background.subscribe_list();
        let runs = runtime.workflows.subscribe();
        let stream = futures::stream::unfold(
            (background, runtime, processes, runs, true),
            |(background, runtime, mut processes, mut runs, first)| async move {
                if !first {
                    let result = {
                        let process_change = processes.changed();
                        let run_change = runs.changed();
                        futures::pin_mut!(process_change, run_change);
                        match futures::future::select(process_change, run_change).await {
                            futures::future::Either::Left((result, _)) => result,
                            futures::future::Either::Right((result, _)) => result,
                        }
                    };
                    // The senders are engine-owned; their drop means the
                    // engine is gone. Both sides are marked seen — a change
                    // that raced in on the other receiver is already in the
                    // snapshot this iteration emits.
                    if result.is_err() {
                        return None;
                    }
                    let _ = processes.borrow_and_update();
                    let _ = runs.borrow_and_update();
                }
                let value = serde_json::to_value(merged_rows(&background, &runtime))
                    .expect("background process list serializes");
                Some((value, (background, runtime, processes, runs, false)))
            },
        );
        RpcReply::Stream(Box::pin(stream))
    }

    pub(super) fn subscribe_background_process(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::background::SubscribeBackgroundProcess =
            serde_json::from_value(params)
                .map_err(|error| RpcError::BadParams(error.to_string()))?;
        self.background
            .subscribe_output(&params.process_id, params.after_seq)
    }

    pub(super) fn kill_background_process(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::background::KillBackgroundProcess = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        self.background.kill(&params.process_id)?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn remove_background_process(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::background::RemoveBackgroundProcess = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        self.background.remove(&params.process_id)?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn clear_exited_background_processes(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::background::ClearExitedBackgroundProcesses =
            serde_json::from_value(params)
                .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let removed = self.background.clear_exited(&params.chat_id);
        RpcReply::value(&serde_json::json!({ "removed": removed }))
    }
}
