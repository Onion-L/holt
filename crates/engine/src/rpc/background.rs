//! Chat-owned background processes over the RPC contract
//! (ADR-background-processes-are-chat-owned).

use holt_rpc::{RpcError, RpcReply};

use crate::EngineService;

impl EngineService {
    /// The live list stream: the current snapshot first, then a fresh
    /// snapshot on every structural change (spawn, exit, removal).
    pub(super) fn watch_background_processes(&self) -> RpcReply {
        let background = self.background.clone();
        let receiver = background.subscribe_list();
        let stream = futures::stream::unfold(
            (background, receiver, true),
            |(background, mut receiver, first)| async move {
                if !first && receiver.changed().await.is_err() {
                    return None;
                }
                let value = serde_json::to_value(background.list())
                    .expect("background process list serializes");
                Some((value, (background, receiver, false)))
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
