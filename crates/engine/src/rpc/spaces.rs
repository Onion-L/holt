//! Space and chat entity mutations — the `Mutate` op surface — plus the
//! git-state refresh the workspace status watch rides on.

use chrono::Utc;
use holt_proto::{Chat, ChatConfig, Space, TitleSource, WorkspaceGitStatus};
use holt_rpc::{RpcError, RpcReply};
use serde::Deserialize;

use super::required_string;
use crate::EngineService;
use crate::store::{persist_chats, persist_spaces};

impl EngineService {
    /// Refresh a registered space after its live Git-status stream observes a
    /// repository appearing or disappearing under the space root.
    pub(super) fn refresh_space_git_state(
        &self,
        root: &std::path::Path,
        snapshot: &WorkspaceGitStatus,
    ) {
        let root = root.to_path_buf();
        let git_dir = snapshot
            .workdir
            .as_deref()
            .and_then(|_| crate::git::discover_git_dir(&root));
        let checkout_id = git_dir
            .as_ref()
            .map(|git_dir| crate::git::checkout_identity(&self.engine_info.device_id, git_dir));
        let mut spaces = self
            .spaces
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let Some(space) = spaces.iter_mut().find(|space| {
            std::path::Path::new(&space.path)
                .canonicalize()
                .map(|path| path == root)
                .unwrap_or(false)
        }) else {
            return;
        };
        let changed = space.git_detected != git_dir.is_some() || space.checkout_id != checkout_id;
        if !changed {
            return;
        }
        space.git_detected = git_dir.is_some();
        space.git_checked_at = Some(Utc::now());
        space.checkout_id = checkout_id;
        if let Err(error) = persist_spaces(&self.data_dir, &spaces) {
            tracing::warn!(%error, "failed to persist refreshed Git space state");
            return;
        }
        if let Ok(value) = serde_json::to_value(&*spaces) {
            self.spaces_tx.send_replace(value);
        }
    }

    pub(super) fn create_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: CreateSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.space_id.trim().is_empty()
            || params.device_id.trim().is_empty()
            || params.path.trim().is_empty()
        {
            return Err(RpcError::BadParams(
                "spaceId, deviceId, and path must not be empty".into(),
            ));
        }
        // The Home row is engine-owned (ADR-0039); it is ensured at boot,
        // never minted through the registry mutate.
        if params.space_id == holt_proto::HOME_SPACE_ID {
            return Err(RpcError::BadParams("the Home space id is reserved".into()));
        }

        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        if spaces.iter().any(|space| {
            space.id == params.space_id
                || (space.device_id == params.device_id && space.path == params.path)
        }) {
            return RpcReply::value(&serde_json::json!({}));
        }
        // Mint the canonical checkout identity at create time (ADR-0002):
        // sha256(deviceId ‖ NUL ‖ git_dir) for folders that are git work
        // trees. A git_detected path that discovers no repo stays untagged.
        let space = Space {
            checkout_id: params
                .git_detected
                .then(|| {
                    crate::git::discover_git_dir(std::path::Path::new(&params.path))
                        .map(|git_dir| crate::git::checkout_identity(&params.device_id, &git_dir))
                })
                .flatten(),
            id: params.space_id,
            device_id: params.device_id,
            path: params.path,
            name: None,
            git_detected: params.git_detected,
            git_checked_at: None,
            created_at: Utc::now(),
        };
        spaces.push(space);
        persist_spaces(&self.data_dir, &spaces).map_err(|error| {
            spaces.pop();
            RpcError::Failed(error.to_string())
        })?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        self.spaces_tx.send_replace(value);
        RpcReply::value(&serde_json::json!({}))
    }

    /// Remove a space (UI "Remove project"): the row goes, and every chat
    /// in the space goes with it — the confirm dialog promises the sessions
    /// are permanently deleted, matching the workspace-doc cascade. Unknown
    /// ids: idempotent no-op, and no watch frame when nothing moved.
    pub(super) fn delete_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: DeleteSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.space_id == holt_proto::HOME_SPACE_ID {
            return Err(RpcError::BadParams(
                "the Home space cannot be deleted".into(),
            ));
        }
        if params.space_id.trim().is_empty() {
            return Err(RpcError::BadParams("spaceId must not be empty".into()));
        }
        let _chats_store = self
            .runtime
            .chats_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let chat_ids: Vec<String> = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = chats
                .iter()
                .filter(|chat| chat.space_id.as_deref() == Some(params.space_id.as_str()))
                .map(|chat| chat.id.clone())
                .collect();
            chats.retain(|chat| chat.space_id.as_deref() != Some(params.space_id.as_str()));
            if !ids.is_empty() {
                crate::store::persist_chats(&self.data_dir, &chats)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
            drop(chats);
            for chat_id in &ids {
                self.runtime.remove_chat(chat_id);
                self.terminals.close_chat(chat_id);
            }
            ids
        };
        if !chat_ids.is_empty() {
            self.runtime.publish_chats();
        }
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        let before = spaces.len();
        spaces.retain(|space| space.id != params.space_id);
        let removed = spaces.len() != before;
        if removed {
            persist_spaces(&self.data_dir, &spaces)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(spaces);
        if removed {
            self.spaces_tx.send_replace(value);
            self.pause_routines_in_space(&params.space_id);
        }
        RpcReply::value(&serde_json::json!({}))
    }

    /// Rename a space (UI "Rename…"): sets the user-visible name override;
    /// unknown ids are an idempotent no-op, matching the chat paths.
    pub(super) fn rename_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: RenameSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let name = params.name.trim();
        if params.space_id.trim().is_empty() || name.is_empty() {
            return Err(RpcError::BadParams(
                "spaceId and name must not be empty".into(),
            ));
        }
        if params.space_id == holt_proto::HOME_SPACE_ID {
            return Err(RpcError::BadParams(
                "the Home space cannot be renamed".into(),
            ));
        }
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        let Some(row) = spaces.iter_mut().find(|space| space.id == params.space_id) else {
            return RpcReply::value(&serde_json::json!({}));
        };
        row.name = Some(name.to_string());
        persist_spaces(&self.data_dir, &spaces)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(spaces);
        self.spaces_tx.send_replace(value);
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn create_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: CreateChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let space_id = params
            .space_id
            .clone()
            .or_else(|| Some(holt_proto::HOME_SPACE_ID.to_string()));
        let space = space_id.as_deref().and_then(|space_id| {
            self.spaces
                .read()
                .ok()?
                .iter()
                .find(|space| space.id == space_id)
                .cloned()
        });
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if chats.iter().any(|chat| chat.id == params.chat_id) {
            return RpcReply::value(&serde_json::json!({}));
        }
        // New chats inherit the last mode used on the device (ADR-0014):
        // the sticky default overrides whatever mode the creating client
        // sent — inheritance is engine-owned, read here at creation.
        let config = params.config.map(|mut config| {
            config.permission_mode = self.mode_default.get();
            config
        });
        chats.push(Chat {
            id: params.chat_id.clone(),
            device_id: params
                .device_id
                .or_else(|| space.as_ref().map(|space| space.device_id.clone()))
                .unwrap_or_else(|| self.engine_info.device_id.clone()),
            title: None,
            title_source: TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: params
                .cwd
                .or_else(|| space.as_ref().map(|space| space.path.clone())),
            branch: params.branch,
            checkout_id: space.as_ref().and_then(|space| space.checkout_id.clone()),
            source_context: None,
            config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            space_id,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            provider_mode: false,
            worktree: params.worktree,
            routine_run: None,
        });
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.chat(&params.chat_id);
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn set_chat_archived(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: SetChatArchivedParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) else {
            // Unknown chat: idempotent no-op, matching create's duplicate path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.archived = params.archived;
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn set_chat_pinned(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: SetChatPinnedParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) else {
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.pinned = params.pinned;
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn delete_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: DeleteChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let _chats_store = self
            .runtime
            .chats_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let before = chats.len();
        chats.retain(|chat| chat.id != params.chat_id);
        let removed = chats.len() != before;
        if removed {
            crate::store::persist_chats(&self.data_dir, &chats)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        drop(chats);
        if removed {
            self.runtime.remove_chat(&params.chat_id);
            self.terminals.close_chat(&params.chat_id);
            self.runtime.publish_chats();
            self.forget_routine_run_chat(&params.chat_id);
            // Reclaim at restart, when no in-memory draft or retry owns files.
        }
        // Unknown chat: idempotent no-op, matching the archive path.
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn rename_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: RenameChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let title = params.title.trim();
        if title.is_empty() {
            return Err(RpcError::BadParams("title must not be empty".into()));
        }
        let title = title.to_string();
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) else {
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.title = Some(title);
        // A manual rename locks the title even when the text is unchanged,
        // so ownership is unambiguous — always persist and publish.
        row.title_source = TitleSource::UserManual;
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn mark_chat_seen(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) else {
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        // Only an unseen row needs a write: re-marks (racing devices, a
        // re-selected row) must not republish the chat list.
        if !row.unseen() {
            return RpcReply::value(&serde_json::json!({}));
        }
        row.last_seen_at = Some(Utc::now());
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) fn set_chat_config(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let mut config: ChatConfig = serde_json::from_value(
            params
                .get("config")
                .cloned()
                .ok_or_else(|| RpcError::BadParams("config is required".into()))?,
        )
        .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let chat = chats
            .iter_mut()
            .find(|chat| chat.id == chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        // Same rule as the run-acceptance write: the permission mode is not
        // this payload's to move (ADR-0014) — the stored mode survives
        // whole-config rewrites, and a config-less row inherits the sticky
        // default instead of whatever tier the writer defaulted.
        config.permission_mode = chat
            .config
            .as_ref()
            .map(|stored| stored.permission_mode)
            .unwrap_or_else(|| self.mode_default.get());
        chat.config = Some(config);
        persist_chats(&self.data_dir, &chats)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(chats);
        self.runtime.publish_chats();
        // The picker moves the occupancy denominator: with an empty queue the
        // selection's window is what the next request is measured against.
        self.refresh_selected_model(chat_id);
        RpcReply::value(&serde_json::json!({}))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSpaceParams {
    space_id: String,
    device_id: String,
    path: String,
    #[serde(default)]
    git_detected: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteSpaceParams {
    space_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameSpaceParams {
    space_id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChatParams {
    chat_id: String,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    config: Option<ChatConfig>,
    /// Session-worktree isolation intent (ADR-0038). Best-effort pre-stamp —
    /// the durable carrier is the queued Run's `WorktreeSpec` absorbed in
    /// `enqueue_run`.
    #[serde(default)]
    worktree: Option<holt_proto::WorktreeSpec>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetChatArchivedParams {
    chat_id: String,
    archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetChatPinnedParams {
    chat_id: String,
    pinned: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteChatParams {
    chat_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameChatParams {
    chat_id: String,
    title: String,
}
