//! The workspace file surface (ADR-0020): roots, listing, reads, writes,
//! entries, and the workspace watches.

use holt_proto::WorkspaceGitStatus;
use holt_rpc::{RpcError, RpcReply};
use serde::Deserialize;

use crate::EngineService;

impl EngineService {
    /// The working directory whose Git state a Turn change set reads: the
    /// chat's stamped cwd, else its space's path.
    pub(super) fn turn_change_root(&self, chat_id: &str) -> Result<String, RpcError> {
        self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: Some(chat_id.to_string()),
            space_id: None,
        })
    }

    /// The directory `SearchFiles` walks: the chat's own cwd when set,
    /// otherwise its space's path; a space id resolves to the space path
    /// directly. Unknown ids are backend faults, not param errors.
    pub(super) fn search_files_root(&self, params: &SearchFilesParams) -> Result<String, RpcError> {
        let root = if let Some(chat_id) = params.chat_id.as_deref() {
            let chats = self
                .runtime
                .chats
                .read()
                .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
            let chat = chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::Failed(format!("unknown chat {chat_id}")))?;
            match chat.cwd.clone() {
                Some(cwd) => cwd,
                None => {
                    let space_id = chat.space_id.clone().ok_or_else(|| {
                        RpcError::Failed(format!("chat {chat_id} has no working directory"))
                    })?;
                    let spaces = self
                        .spaces
                        .read()
                        .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
                    spaces
                        .iter()
                        .find(|space| space.id == space_id)
                        .map(|space| space.path.clone())
                        .ok_or_else(|| RpcError::Failed(format!("unknown space {space_id}")))?
                }
            }
        } else {
            let space_id = params.space_id.as_deref().expect("selector checked");
            let spaces = self
                .spaces
                .read()
                .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
            spaces
                .iter()
                .find(|space| space.id == space_id)
                .map(|space| space.path.clone())
                .ok_or_else(|| RpcError::Failed(format!("unknown space {space_id}")))?
        };
        Ok(crate::local_fs::expand_tilde(&root))
    }

    // Fuzzy path search for the composer's `@`-mention palette: the
    // root comes from chat/space state, the walk+match runs off the
    // async workers.
    pub(super) async fn search_files(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: SearchFilesParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&params)?;
        let query = params.query.clone();
        let matches = tokio::task::spawn_blocking(move || {
            crate::path_search::search(std::path::Path::new(&root), &query)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("search task failed: {error}")))?;
        RpcReply::value(&matches)
    }

    // File sidebar (ADR-0020 groundwork): one directory level and
    // bounded text reads, fenced behind the owning space's root. The
    // blocking FS work runs off the async workers like SearchFiles.
    pub(super) async fn list_workspace_entries(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WorkspacePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        params.check_selector()?;
        let root = self.search_files_root(&params.as_search_root())?;
        let requested = params.path.clone().unwrap_or_default();
        let listing = tokio::task::spawn_blocking(move || {
            crate::files::list_directory(std::path::Path::new(&root), &requested)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("listing task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&listing)
    }

    pub(super) async fn read_workspace_file(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WorkspacePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        params.check_selector()?;
        let path = params.require_path()?;
        let root = self.search_files_root(&params.as_search_root())?;
        // Skill roots ride along: a personal/holt `SKILL.md` opens
        // in a sidebar file tab via its absolute catalog path.
        let skill_roots = self.skills.out_of_workspace_roots();
        let read = tokio::task::spawn_blocking(move || {
            crate::files::read_file(std::path::Path::new(&root), &path, &skill_roots)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("read task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&read)
    }

    pub(super) async fn read_workspace_image(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WorkspacePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        params.check_selector()?;
        let path = params.require_path()?;
        let root = self.search_files_root(&params.as_search_root())?;
        // The fence (root containment, `.git`, symlink landing paths)
        // runs before any bytes move; the bounded sniffed read itself
        // is the images store's, under the same limits as ReadImage.
        let canonical = tokio::task::spawn_blocking(move || {
            crate::files::resolve_image_target(std::path::Path::new(&root), &path)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("image read task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        let display = canonical.display().to_string();
        let (mime_type, data) = self.images.read(&display).await.map_err(RpcError::Failed)?;
        RpcReply::value(&holt_rpc::images::WorkspaceImageData {
            path: display,
            mime_type,
            data,
        })
    }

    pub(super) async fn save_workspace_file(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: SaveWorkspaceFileParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let (path, text, version, expect_disk_version, bom) = (
            params.path,
            params.text,
            params.version,
            params.expect_disk_version,
            params.bom,
        );
        let save = tokio::task::spawn_blocking(move || {
            crate::files::save_file(
                std::path::Path::new(&root),
                &path,
                &text,
                &version,
                expect_disk_version.as_deref(),
                bom,
            )
        })
        .await
        .map_err(|error| RpcError::Failed(format!("save task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&save)
    }

    pub(super) async fn write_workspace_file_as(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WriteFileAsParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let (path, text, bom) = (params.path, params.text, params.bom);
        let saved = tokio::task::spawn_blocking(move || {
            crate::files::write_file_as(std::path::Path::new(&root), &path, &text, bom)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("save-as task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&saved)
    }

    pub(super) fn watch_workspace_entries(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WorkspacePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        params.check_selector()?;
        let root = self.search_files_root(&params.as_search_root())?;
        let canonical = std::path::Path::new(&root)
            .canonicalize()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let stream = crate::workspace_watch::subscribe(canonical).map_err(RpcError::Failed)?;
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    // Working-tree Git status decorations (file-sidebar ticket 10):
    // a focused extension of the git watch — a fresh snapshot after
    // every change under the selector's space root (working tree or
    // `.git`), keyed to that Space's folder, never a diff scope.
    pub(super) fn watch_workspace_git_status(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: WorkspacePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        params.check_selector()?;
        let root = self.search_files_root(&params.as_search_root())?;
        let canonical = std::path::Path::new(&root)
            .canonicalize()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let stream = crate::git_status_watch::subscribe(canonical.clone(), self.git.clone())
            .map_err(RpcError::Failed)?;
        use futures::StreamExt;
        let service = self.clone();
        let stream = stream.map(move |value| {
            if let Ok(snapshot) = serde_json::from_value::<WorkspaceGitStatus>(value.clone()) {
                service.refresh_space_git_state(&canonical, &snapshot);
            }
            value
        });
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    pub(super) async fn create_workspace_entry(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: CreateEntryParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let (parent, name, is_dir) = (
            params.parent_path.unwrap_or_default(),
            params.name,
            params.is_dir,
        );
        tokio::task::spawn_blocking(move || {
            crate::files::create_entry(std::path::Path::new(&root), &parent, &name, is_dir)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("create task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) async fn rename_workspace_entry(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: RenameEntryParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let (path, new_name) = (params.path, params.new_name);
        let destination = tokio::task::spawn_blocking(move || {
            crate::files::rename_entry(std::path::Path::new(&root), &path, &new_name)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("rename task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&serde_json::json!({ "path": destination }))
    }

    pub(super) async fn move_workspace_entry(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: MoveEntryParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let (path, destination_directory) = (params.path, params.destination_directory);
        let destination = tokio::task::spawn_blocking(move || {
            crate::files::move_entry(std::path::Path::new(&root), &path, &destination_directory)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("move task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&serde_json::json!({ "path": destination }))
    }

    pub(super) async fn trash_workspace_entry(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: TrashEntryParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.is_some() == params.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        let root = self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: params.chat_id,
            space_id: params.space_id,
        })?;
        let path = params.path;
        tokio::task::spawn_blocking(move || {
            crate::files::trash_entry(std::path::Path::new(&root), &path)
        })
        .await
        .map_err(|error| RpcError::Failed(format!("trash task failed: {error}")))?
        .map_err(|fault| RpcError::Failed(fault.to_string()))?;
        RpcReply::value(&serde_json::json!({}))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchFilesParams {
    pub(super) query: String,
    #[serde(default)]
    pub(super) chat_id: Option<String>,
    #[serde(default)]
    pub(super) space_id: Option<String>,
}

/// Selector + path shape shared by the File-sidebar workspace methods.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspacePathParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveWorkspaceFileParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    text: String,
    /// The disk version token the draft was based on.
    version: String,
    /// A confirmed overwrite's reviewed disk token (ticket 04): when set,
    /// the save applies only if the disk STILL holds exactly that version.
    #[serde(default)]
    expect_disk_version: Option<String>,
    #[serde(default)]
    bom: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    parent_path: Option<String>,
    name: String,
    #[serde(default)]
    is_dir: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    new_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    #[serde(default)]
    destination_directory: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrashEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileAsParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    text: String,
    #[serde(default)]
    bom: bool,
}

impl WorkspacePathParams {
    /// Exactly one of chatId/spaceId, mirroring `SearchFiles`.
    fn check_selector(&self) -> Result<(), RpcError> {
        if self.chat_id.is_some() == self.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        Ok(())
    }

    /// The non-empty path the read family (text and image) requires.
    fn require_path(&self) -> Result<String, RpcError> {
        self.path
            .clone()
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| RpcError::BadParams("path is required".into()))
    }

    fn as_search_root(&self) -> SearchFilesParams {
        SearchFilesParams {
            query: String::new(),
            chat_id: self.chat_id.clone(),
            space_id: self.space_id.clone(),
        }
    }
}
