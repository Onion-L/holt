//! holt-engine — the backend slot for the desktop shell.
//!
//! The original engine (agent orchestration, doc host, sync, auth, terminals,
//! repos, uploads) was removed when this repo became a UI shell. What remains:
//!
//! - [`StubEngine`] — an [`RpcService`] answering the RPC surface the UI speaks
//!   with empty catalogs, empty watch streams, and unknown-method errors for
//!   anything it has no backend for. It is
//!   the placeholder a real backend (e.g. pi-core-rs) replaces: implement the
//!   methods in [`StubEngine::handle`], keep the reply shapes, and the whole UI
//!   lights up without further changes.
//! - [`InstanceLock`] — single-instance guard on the data dir.
//! - [`registry`] — the harness-descriptor types the settings/picker UI reads.

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
};

use async_trait::async_trait;
use chrono::Utc;
use holt_proto::{AuthState, Device, DriveEntry, DriveListing, FolderEntry, FolderListing, Space};
pub use holt_proto::{EngineInfo, HarnessId, WorkspaceScope};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde::Deserialize;
use tokio::sync::watch;

pub mod instance_lock;
pub mod registry;

pub use instance_lock::InstanceLock;
pub use registry::{HarnessDescriptor, descriptor_enabled, descriptors};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Everything the stub backend needs. A real engine grows this back
/// (harness config, IPC port, …) as it needs it.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.holt`).
    pub data_dir: PathBuf,
}

/// The no-op backend. Serves the RPC method surface with empty data so the
/// shell boots: no chats, no spaces, no devices, no harnesses.
pub struct StubEngine {
    engine_info: EngineInfo,
    data_dir: PathBuf,
    spaces: RwLock<Vec<Space>>,
    spaces_tx: watch::Sender<serde_json::Value>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl StubEngine {
    /// Assemble the stub against a data dir. Takes the instance lock and
    /// resolves a stable device id.
    pub fn assemble(config: &EngineConfig) -> Result<Self, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let device_id = load_or_create_device_id(&config.data_dir)?;
        let spaces = load_spaces(&config.data_dir)?;
        let spaces_value =
            serde_json::to_value(&spaces).map_err(|error| EngineError::Other(error.to_string()))?;
        let (spaces_tx, _) = watch::channel(spaces_value);
        Ok(Self {
            engine_info: EngineInfo {
                device_id,
                workspace_scope: WorkspaceScope::Local,
            },
            data_dir: config.data_dir.clone(),
            spaces: RwLock::new(spaces),
            spaces_tx,
            _instance_lock: lock,
        })
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    fn watch_spaces(&self) -> RpcReply {
        let receiver = self.spaces_tx.subscribe();
        let stream =
            futures::stream::unfold((receiver, true), |(mut receiver, first)| async move {
                if !first && receiver.changed().await.is_err() {
                    return None;
                }
                let value = receiver.borrow().clone();
                Some((value, (receiver, false)))
            });
        RpcReply::Stream(Box::pin(stream))
    }

    fn create_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
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
        spaces.push(Space {
            id: params.space_id,
            device_id: params.device_id,
            path: params.path,
            name: None,
            git_detected: params.git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        });
        persist_spaces(&self.data_dir, &spaces).map_err(|error| {
            spaces.pop();
            RpcError::Failed(error.to_string())
        })?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        self.spaces_tx.send_replace(value);
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

fn spaces_path(data_dir: &Path) -> PathBuf {
    data_dir.join("spaces.json")
}

fn load_spaces(data_dir: &Path) -> Result<Vec<Space>, EngineError> {
    let path = spaces_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn persist_spaces(data_dir: &Path, spaces: &[Space]) -> Result<(), EngineError> {
    let path = spaces_path(data_dir);
    let temp_path = data_dir.join("spaces.json.tmp");
    let bytes =
        serde_json::to_vec_pretty(spaces).map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

/// A watch stream that emits `value` once, then stays open (never changes).
fn static_watch(value: serde_json::Value) -> RpcReply {
    use futures::StreamExt;
    RpcReply::Stream(
        futures::stream::once(futures::future::ready(value))
            .chain(futures::stream::pending::<serde_json::Value>())
            .boxed(),
    )
}

/// A stream that never emits — for subscriptions the stub has no data for.
fn pending_stream() -> RpcReply {
    use futures::StreamExt;
    RpcReply::Stream(futures::stream::pending::<serde_json::Value>().boxed())
}

/// The local machine as a device row — enough for the add-space palette to
/// browse this computer (presence for the local device is always "online").
fn local_device(device_id: &str) -> Device {
    Device {
        id: device_id.to_string(),
        name: hostname(),
        platform: std::env::consts::OS.to_string(),
        last_seen_at: None,
        created_at: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
}

/// Bare hostname without any domain suffix ("macbook.local" → "macbook").
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]);
        let name = name.split('.').next().unwrap_or("").trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "This device".to_string()
}

fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .map(|home| home.to_string_lossy().to_string())
        .filter(|home| !home.is_empty())
}

/// The UI expands `~` itself; tolerate it here anyway so the method is
/// callable without the shell's helpers.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Browse cap — the palette scrolls, but a runaway directory (think `/`) is
/// still bounded; `truncated` tells the UI some entries were dropped.
const FOLDER_ENTRY_CAP: usize = 500;

fn list_folders(params: &serde_json::Value) -> Result<FolderListing, String> {
    let requested = params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let path = match requested {
        Some(path) => expand_tilde(path),
        None => home_dir().ok_or_else(|| "could not resolve your home folder".to_string())?,
    };
    let read =
        std::fs::read_dir(&path).map_err(|error| format!("could not read that folder: {error}"))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().to_string();
        // Dotfiles stay hidden — the browser is for project folders.
        if name.starts_with('.') {
            continue;
        }
        // `std::fs::metadata` (not `item.metadata`) so a symlinked folder
        // still counts as a folder.
        let Ok(meta) = std::fs::metadata(item.path()) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if entries.len() >= FOLDER_ENTRY_CAP {
            truncated = true;
            break;
        }
        entries.push(FolderEntry {
            is_repo: item.path().join(".git").exists(),
            name,
            is_dir: true,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(FolderListing {
        path,
        entries,
        truncated,
    })
}

/// Browse roots beyond home: the system root plus mounted volumes. macOS
/// keeps mounts under `/Volumes`; the boot volume's alias there resolves to
/// `/`, so it is deduped against the System row.
fn list_drives() -> DriveListing {
    let mut drives = vec![DriveEntry {
        name: "System".to_string(),
        path: "/".to_string(),
    }];
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for item in read.flatten() {
            let path = item.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if std::fs::canonicalize(&path).is_ok_and(|canon| canon == Path::new("/")) {
                continue;
            }
            drives.push(DriveEntry {
                name: item.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
    DriveListing { drives }
}

#[async_trait]
impl RpcService for StubEngine {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => RpcReply::value(&serde_json::json!({ "ready": true })),
            methods::LOCAL_DEVICE => RpcReply::value(&serde_json::json!({
                "deviceId": self.engine_info.device_id,
            })),
            methods::LIST_HARNESSES | methods::SET_HARNESS_ENABLED => {
                RpcReply::value(&descriptors())
            }
            methods::LIST_MODELS | methods::LIST_COMMANDS => {
                RpcReply::value(&serde_json::json!([]))
            }
            // Local folder browsing for the add-space palette. The UI only
            // targets remote devices over the relay; here every browse is local.
            methods::LIST_FOLDERS => match list_folders(&params) {
                Ok(listing) => RpcReply::value(&listing),
                Err(message) => Err(RpcError::Failed(message)),
            },
            methods::LIST_DRIVES => RpcReply::value(&list_drives()),

            // Entity watches: one snapshot, then silence. Devices are real —
            // the local machine browses its own folders — the rest stay empty.
            methods::WATCH_DEVICES => {
                let value = serde_json::to_value(vec![local_device(&self.engine_info.device_id)])
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }
            methods::WATCH_CHATS | methods::WATCH_SESSIONS | methods::WATCH_TRANSFERS => {
                Ok(static_watch(serde_json::json!([])))
            }
            methods::WATCH_SPACES => Ok(self.watch_spaces()),
            methods::WATCH_CONNECTIVITY => {
                // Default = state Disabled ("no edge transports on this
                // profile — hide the pill"), no chat rooms.
                let value = serde_json::to_value(holt_proto::Connectivity::default())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }
            methods::AUTH_STATUS => {
                let value = serde_json::to_value(AuthState::SignedOut)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }

            // Transcript / terminal data: nothing to serve.
            methods::WATCH_DOC_MESSAGES
            | methods::SUBSCRIBE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::WATCH_CHECKOUT_CHANGE_REQUEST => Ok(pending_stream()),

            // No-op liveness pokes the UI fires defensively.
            methods::PROBE_SYNC => RpcReply::value(&serde_json::json!({})),

            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("createSpace") =>
            {
                self.create_space(params)
            }

            // Everything the stub has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_across_assembles() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let first = StubEngine::assemble(&config).unwrap();
        let id = first.engine_info().device_id.clone();
        // The lock dies with the engine; a fresh assemble reads the same id.
        drop(first);
        let second = StubEngine::assemble(&config).unwrap();
        assert_eq!(second.engine_info().device_id, id);
    }

    #[test]
    fn second_engine_on_one_data_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let _first = StubEngine::assemble(&config).unwrap();
        assert!(StubEngine::assemble(&config).is_err());
    }

    #[test]
    fn list_folders_returns_dirs_sorted_and_marks_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("zed")).unwrap();
        std::fs::create_dir(root.join("Alpha")).unwrap();
        std::fs::create_dir(root.join("repo")).unwrap();
        std::fs::create_dir(root.join("repo/.git")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("notes.txt"), "hi").unwrap();
        let listing = list_folders(&serde_json::json!({
            "path": root.to_string_lossy(),
        }))
        .unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "repo", "zed"]);
        assert!(listing.entries.iter().all(|e| e.is_dir));
        assert!(listing.entries[1].is_repo);
        assert!(!listing.entries[0].is_repo);
        assert!(!listing.truncated);
    }

    #[test]
    fn list_folders_errors_read_like_folder_failures() {
        // The UI distinguishes folder-level failures by the word "folder".
        let error =
            list_folders(&serde_json::json!({ "path": "/definitely/not/here" })).unwrap_err();
        assert!(error.contains("folder"), "unexpected message: {error}");
    }

    #[test]
    fn expand_tilde_resolves_against_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/dev"), format!("{home}/dev"));
        assert_eq!(expand_tilde("/abs"), "/abs");
    }

    #[test]
    fn list_drives_always_offers_the_system_root() {
        let drives = list_drives();
        assert!(drives.drives.iter().any(|d| d.path == "/"));
        // The boot volume's /Volumes alias must not duplicate the System row.
        assert!(
            !drives.drives.iter().any(|d| d.path != "/"
                && std::fs::canonicalize(&d.path).is_ok_and(|p| p == Path::new("/")))
        );
    }

    #[tokio::test]
    async fn create_space_updates_watch_and_survives_restart() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut spaces) = engine
            .handle(methods::WATCH_SPACES, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchSpaces did not return a stream");
        };
        assert_eq!(spaces.next().await.unwrap(), serde_json::json!([]));

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createSpace",
                    "spaceId": "space-1",
                    "deviceId": engine.engine_info().device_id,
                    "path": "/tmp/project",
                    "gitDetected": true,
                }),
            )
            .await
            .unwrap();

        let update = spaces.next().await.unwrap();
        assert_eq!(update.as_array().unwrap().len(), 1);
        assert_eq!(update[0]["id"], "space-1");
        assert_eq!(update[0]["path"], "/tmp/project");
        drop(spaces);
        drop(engine);

        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut spaces) = engine
            .handle(methods::WATCH_SPACES, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchSpaces did not return a stream");
        };
        let restored = spaces.next().await.unwrap();
        assert_eq!(restored.as_array().unwrap().len(), 1);
        assert_eq!(restored[0]["id"], "space-1");
    }
}
