//! Chat-owned terminals over the RPC contract (ADR-0017).

use holt_rpc::{RpcError, RpcReply, methods};

use super::workspace::SearchFilesParams;
use crate::EngineService;

impl EngineService {
    pub(super) async fn open_terminal(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::terminals::OpenTerminal = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let known_chat = self
            .runtime
            .chats
            .read()
            .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?
            .iter()
            .any(|chat| chat.id == params.chat_id);
        // Chat-owned PTYs root at the chat's working directory (the
        // existing resolution, errors included). Chat-less terminals
        // — the new-chat canvas or the no-project empty state — take
        // the caller's explicit cwd, else the user's home directory.
        let cwd = if known_chat {
            self.search_files_root(&SearchFilesParams {
                chat_id: Some(params.chat_id.clone()),
                space_id: None,
                query: String::new(),
            })?
        } else {
            chatless_terminal_root(params.cwd.clone())?
        };
        let terminals = self.terminals.clone();
        let session = tokio::task::spawn_blocking(move || {
            terminals.open(params.chat_id, &cwd, params.cols, params.rows)
        })
        .await
        .map_err(|e| RpcError::Failed(e.to_string()))??;
        RpcReply::value(&session)
    }

    pub(super) fn subscribe_terminal(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::terminals::SubscribeTerminal = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        self.terminals
            .subscribe(&params.terminal_id, params.after_seq)
    }

    pub(super) async fn write_terminal(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::terminals::WriteTerminal = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let terminals = self.terminals.clone();
        tokio::task::spawn_blocking(move || terminals.write(&params.terminal_id, &params.data))
            .await
            .map_err(|e| RpcError::Failed(e.to_string()))??;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn resize_terminal(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::terminals::ResizeTerminal = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        self.terminals
            .resize(&params.terminal_id, params.cols, params.rows)?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) async fn close_terminal(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let id = if method == methods::CLOSE_TERMINAL {
            Some(
                serde_json::from_value::<holt_rpc::terminals::TerminalId>(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?
                    .terminal_id,
            )
        } else {
            None
        };
        let terminals = self.terminals.clone();
        tokio::task::spawn_blocking(move || match id {
            Some(id) => terminals.close(&id),
            None => terminals.close_all(false),
        })
        .await
        .map_err(|e| RpcError::Failed(e.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }
}

/// The chat-less terminal root (`OpenTerminal`): the caller's explicit cwd,
/// else the user's home directory. An empty override means "not given".
fn chatless_terminal_root(cwd: Option<String>) -> Result<String, RpcError> {
    match cwd.filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => Ok(cwd),
        None => crate::local_fs::home_dir()
            .ok_or_else(|| RpcError::Failed("could not resolve your home folder".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::chatless_terminal_root;

    #[test]
    fn chatless_terminal_root_prefers_the_explicit_cwd() {
        assert_eq!(
            chatless_terminal_root(Some("/tmp/holt-root".into())).unwrap(),
            "/tmp/holt-root"
        );
        let home = chatless_terminal_root(None).unwrap();
        assert!(!home.is_empty());
        assert_eq!(chatless_terminal_root(Some(String::new())).unwrap(), home);
    }
}
