//! Port of `pi-core/agent/src/harness/session/jsonl/repo.ts`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::agent::harness::types::{CreateDirOptions, FileSystem, RemoveOptions};
use crate::ai::utils::uuid::uuidv7;

use super::super::memory::{Session, now_millis};
use super::super::types::{ForkOptions, SessionError, SessionErrorCode};
use super::codec::{metadata_from_header, parse_header};
use super::errors::{file_err, file_result};
use super::storage::JsonlSessionStorage;
use super::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlV4Header,
};

/// Port of `SESSION_ID_PATTERN` validation.
fn validate_session_id(id: &str) -> Result<(), SessionError> {
    let valid = !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err(SessionError::new(
            SessionErrorCode::InvalidPayload,
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    Ok(())
}

/// Port of `jsonlSessionDirectoryName`.
fn jsonl_session_directory_name(cwd: &str) -> String {
    let stripped = cwd
        .strip_prefix('/')
        .or_else(|| cwd.strip_prefix('\\'))
        .unwrap_or(cwd);
    format!("--{}--", stripped.replace(['/', '\\', ':'], "-"))
}

/// Port of `sessionFileName`: ISO timestamp with `:` and `.` replaced.
pub fn session_file_name(created_at: i64, id: &str) -> String {
    let iso = iso_timestamp(created_at);
    format!("{}_{}.jsonl", iso.replace([':', '.'], "-"), id)
}

/// UTC ISO-8601 millisecond timestamp (`new Date(ts).toISOString()`).
fn iso_timestamp(timestamp_ms: i64) -> String {
    let seconds = timestamp_ms.div_euclid(1000);
    let millis = timestamp_ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

async fn jsonl_sessions_root(options: &JsonlSessionRepoOptions) -> Result<String, SessionError> {
    file_result(
        options.fs.absolute_path(&options.sessions_root, None).await,
        &format!("Failed to resolve sessions root {}", options.sessions_root),
    )
}

async fn jsonl_session_directory(
    fs: &Arc<dyn FileSystem>,
    sessions_root: &str,
    cwd: &str,
) -> Result<String, SessionError> {
    file_result(
        fs.join_path(
            &[sessions_root.to_string(), jsonl_session_directory_name(cwd)],
            None,
        )
        .await,
        &format!("Failed to resolve sessions directory for {cwd}"),
    )
}

async fn jsonl_session_directories(
    options: &JsonlSessionRepoOptions,
    cwd: Option<&str>,
) -> Result<Vec<String>, SessionError> {
    let sessions_root = jsonl_sessions_root(options).await?;
    if let Some(cwd) = cwd {
        let resolved_cwd = file_result(
            options.fs.absolute_path(cwd, None).await,
            &format!("Failed to resolve session cwd {cwd}"),
        )?;
        let directory = jsonl_session_directory(&options.fs, &sessions_root, &resolved_cwd).await?;
        return Ok(
            if file_result(
                options.fs.exists(&directory, None).await,
                &format!("Failed to check sessions directory {directory}"),
            )? {
                vec![directory]
            } else {
                Vec::new()
            },
        );
    }
    if !file_result(
        options.fs.exists(&sessions_root, None).await,
        &format!("Failed to check sessions directory {sessions_root}"),
    )? {
        return Ok(Vec::new());
    }
    Ok(file_result(
        options.fs.list_dir(&sessions_root, None).await,
        &format!("Failed to list sessions directory {sessions_root}"),
    )?
    .into_iter()
    .filter(|entry| {
        entry.kind == crate::agent::harness::types::FileKind::Directory
            || entry.kind == crate::agent::harness::types::FileKind::Symlink
    })
    .map(|entry| entry.path)
    .collect())
}

/// Port of `listJsonlSessionMetadata`.
pub async fn list_jsonl_session_metadata(
    options: &JsonlSessionRepoOptions,
    query: &JsonlSessionListOptions,
) -> Result<Vec<JsonlSessionMetadata>, SessionError> {
    let mut metadata: Vec<JsonlSessionMetadata> = Vec::new();
    for directory in jsonl_session_directories(options, query.cwd.as_deref()).await? {
        let files: Vec<_> = file_result(
            options.fs.list_dir(&directory, None).await,
            &format!("Failed to list sessions directory {directory}"),
        )?
        .into_iter()
        .filter(|entry| {
            entry.kind != crate::agent::harness::types::FileKind::Directory
                && entry.name.ends_with(".jsonl")
        })
        .collect();
        for file in files {
            let first_line = file_result(
                options
                    .fs
                    .read_text_lines(
                        &file.path,
                        crate::agent::harness::types::ReadTextLinesOptions { max_lines: Some(1) },
                        None,
                    )
                    .await,
                &format!("Failed to read session header {}", file.path),
            )?
            .into_iter()
            .next();
            let Some(first_line) = first_line else {
                continue;
            };
            let Ok(header) = parse_header(&first_line) else {
                continue;
            };
            metadata.push(metadata_from_header(
                &header,
                file.path.clone(),
                file.mtime_ms,
            ));
        }
    }
    metadata.sort_by(|left, right| {
        right
            .modified_at
            .partial_cmp(&left.modified_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(metadata)
}

/// Port of `loadJsonlSessionStorage`.
pub async fn load_jsonl_session_storage(
    options: &JsonlSessionRepoOptions,
    metadata: &JsonlSessionMetadata,
) -> Result<Arc<JsonlSessionStorage>, SessionError> {
    if !file_result(
        options.fs.exists(&metadata.path, None).await,
        &format!("Failed to check session {}", metadata.path),
    )? {
        return Err(SessionError::new(
            SessionErrorCode::NotFound,
            format!("Session not found: {}", metadata.id),
        ));
    }
    let storage = JsonlSessionStorage::load(Arc::clone(&options.fs), &metadata.path).await?;
    let loaded_metadata = storage.jsonl_metadata();
    if loaded_metadata.id != metadata.id {
        return Err(SessionError::new(
            SessionErrorCode::InvalidEntry,
            format!("Session id does not match header: {}", metadata.id),
        ));
    }
    Ok(storage)
}

/// Port of `JsonlSessionRepo`.
pub struct JsonlSessionRepo {
    fs: Arc<dyn FileSystem>,
    sessions_root_input: String,
    active_create_destinations: Mutex<HashSet<String>>,
    root_cache: Mutex<Option<String>>,
}

impl JsonlSessionRepo {
    pub fn new(options: JsonlSessionRepoOptions) -> Self {
        Self {
            fs: options.fs,
            sessions_root_input: options.sessions_root,
            active_create_destinations: Mutex::new(HashSet::new()),
            root_cache: Mutex::new(None),
        }
    }

    /// Port of `create`.
    pub async fn create(
        &self,
        options: JsonlSessionCreateOptions,
    ) -> Result<Session, SessionError> {
        let destination = self.resolve_create_destination(&options).await?;
        self.claim_create_destination(&destination, || async {
            let (header, path) = self.prepare_create(&destination, &options).await?;
            let storage = JsonlSessionStorage::create(Arc::clone(&self.fs), &path, header).await?;
            Ok(Session::new(storage))
        })
        .await
    }

    /// Port of `open`.
    pub async fn open(&self, metadata: JsonlSessionMetadata) -> Result<Session, SessionError> {
        let storage = self.load_storage(&metadata).await?;
        Ok(Session::new(storage))
    }

    /// Port of `list`.
    pub async fn list(
        &self,
        options: &JsonlSessionListOptions,
    ) -> Result<Vec<JsonlSessionMetadata>, SessionError> {
        list_jsonl_session_metadata(
            &JsonlSessionRepoOptions {
                fs: Arc::clone(&self.fs),
                sessions_root: self.sessions_root_input.clone(),
            },
            options,
        )
        .await
    }

    /// Port of `delete`.
    pub async fn delete(&self, metadata: &JsonlSessionMetadata) -> Result<(), SessionError> {
        self.fs
            .remove(
                &metadata.path,
                RemoveOptions {
                    force: Some(true),
                    recursive: None,
                },
                None,
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                file_err(
                    error,
                    &format!("Failed to delete session {}", metadata.path),
                )
            })
    }

    /// Port of `fork`.
    pub async fn fork(
        &self,
        source: JsonlSessionMetadata,
        options: ForkOptions,
        mut create_options: JsonlSessionCreateOptions,
    ) -> Result<Session, SessionError> {
        let source_storage = self.load_storage(&source).await?;
        if create_options.parent_session_id.is_none() {
            create_options.parent_session_id = Some(source.id.clone());
        }
        let destination = self.resolve_create_destination(&create_options).await?;
        self.claim_create_destination(&destination, || async {
            let (header, path) = self.prepare_create(&destination, &create_options).await?;
            let forked = source_storage.fork(&path, header, &options).await?;
            Ok(Session::new(forked))
        })
        .await
    }

    async fn load_storage(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> Result<Arc<JsonlSessionStorage>, SessionError> {
        load_jsonl_session_storage(
            &JsonlSessionRepoOptions {
                fs: Arc::clone(&self.fs),
                sessions_root: self.sessions_root_input.clone(),
            },
            metadata,
        )
        .await
    }

    async fn resolve_create_destination(
        &self,
        options: &JsonlSessionCreateOptions,
    ) -> Result<CreateDestination, SessionError> {
        let id = options.id.clone().unwrap_or_else(uuidv7);
        validate_session_id(&id)?;
        let cwd = file_result(
            self.fs.absolute_path(&options.cwd, None).await,
            &format!("Failed to resolve session cwd {}", options.cwd),
        )?;
        Ok(CreateDestination { id, cwd })
    }

    /// Port of `claimCreateDestination`: prevents same-process create and
    /// fork races for one logical destination.
    async fn claim_create_destination<T, F, Fut>(
        &self,
        destination: &CreateDestination,
        operation: F,
    ) -> Result<T, SessionError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, SessionError>>,
    {
        let key = format!("{}\0{}", destination.cwd, destination.id);
        {
            let mut active = self
                .active_create_destinations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if active.contains(&key) {
                return Err(SessionError::new(
                    SessionErrorCode::AlreadyExists,
                    format!("Session already exists: {}", destination.id),
                ));
            }
            active.insert(key);
        }
        let result = operation().await;
        self.active_create_destinations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&format!("{}\0{}", destination.cwd, destination.id));
        result
    }

    async fn prepare_create(
        &self,
        destination: &CreateDestination,
        options: &JsonlSessionCreateOptions,
    ) -> Result<(JsonlV4Header, String), SessionError> {
        let CreateDestination { id, cwd } = destination;
        if self.session_id_exists(id, cwd).await? {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }

        let created_at = now_millis();
        let session_directory = self.session_directory(cwd).await?;
        let path = file_result(
            self.fs
                .join_path(
                    &[session_directory.clone(), session_file_name(created_at, id)],
                    None,
                )
                .await,
            &format!("Failed to resolve path for session {id}"),
        )?;
        let header = JsonlV4Header {
            version: 4,
            id: id.clone(),
            created_at,
            cwd: cwd.clone(),
            parent_session_id: options.parent_session_id.clone(),
            legacy_parent_session_path: None,
            metadata: options.metadata.clone(),
        };
        self.fs
            .create_dir(
                &session_directory,
                CreateDirOptions {
                    recursive: Some(true),
                },
                None,
            )
            .await
            .map_err(|error| file_err(error, "Failed to create sessions directory"))?;
        Ok((header, path))
    }

    async fn session_id_exists(&self, id: &str, cwd: &str) -> Result<bool, SessionError> {
        let suffix = format!("_{id}.jsonl");
        let directory = self.session_directory(cwd).await?;
        if !file_result(
            self.fs.exists(&directory, None).await,
            &format!("Failed to check sessions directory {directory}"),
        )? {
            return Ok(false);
        }
        let files = file_result(
            self.fs.list_dir(&directory, None).await,
            &format!("Failed to list sessions directory {directory}"),
        )?;
        Ok(files.iter().any(|entry| {
            entry.kind != crate::agent::harness::types::FileKind::Directory
                && entry.name.ends_with(&suffix)
        }))
    }

    async fn session_directory(&self, cwd: &str) -> Result<String, SessionError> {
        let root = self.root().await?;
        file_result(
            self.fs
                .join_path(&[root, jsonl_session_directory_name(cwd)], None)
                .await,
            &format!("Failed to resolve sessions directory for {cwd}"),
        )
    }

    async fn root(&self) -> Result<String, SessionError> {
        if let Some(root) = self
            .root_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Ok(root);
        }
        let root = file_result(
            self.fs.absolute_path(&self.sessions_root_input, None).await,
            &format!(
                "Failed to resolve sessions root {}",
                self.sessions_root_input
            ),
        )?;
        *self
            .root_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root.clone());
        Ok(root)
    }
}

struct CreateDestination {
    id: String,
    cwd: String,
}
