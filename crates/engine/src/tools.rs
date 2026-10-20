//! The agent's execution tools: the holt-core built-ins (read/write/edit/
//! bash) mounted onto the local machine — a [`LocalExecutionEnv`] satisfying
//! holt-core's `FileSystem + Shell` contract, and the assembly that hands each
//! harness tool the shared [`ExecutionToolContext`] its `execute` downcasts
//! for — plus holt's own content search (ADR-in-process-ripgrep-for-content-search) in [`grep`], directory
//! listing (ADR-plan-mode-is-an-independent-approval-checkpoint's read-only surface) in [`ls`], web fetch (ADR-web-tools-full-text-ungated-pluggable-search) in
//! [`web_fetch`], and web search behind the user-configured [`SearchBackend`]
//! (ADR-web-tools-full-text-ungated-pluggable-search) in [`web_search`] — plus the question card's `ask_user` tool
//! (ADR-ask-user-is-a-question-card) in [`ask_user`] and the Turn task list's `todo` tool
//! (issue #47) in [`todo`].

pub(crate) mod ask_user;
mod grep;
mod ls;
pub(crate) mod model_setup;
mod read_chat;
pub(crate) mod submit_plan;
#[cfg(test)]
pub(crate) mod test_http;
pub(crate) mod todo;
mod web_fetch;
pub(crate) mod web_search;

use crate::checkpoints::RunningCall;
use crate::git::{CallStart, CheckpointFile, CheckpointSide, Git, WindowStart};
use crate::shell_env::login_shell;
use crate::turn_changes::Attribution;
use std::{
    future::pending,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use futures::future::BoxFuture;
use holt_core::agent::{
    harness::{
        tools::{
            bash::{BashToolOptions, create_bash_tool},
            edit::create_edit_tool,
            read::{ReadImageProcessorResult, ReadToolOptions, create_read_tool},
            tool_context::ExecutionToolContext,
            write::create_write_tool,
        },
        types::{
            AgentHarnessTool, AgentToolContext, CreateDirOptions, CreateTempFileOptions,
            ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo,
            FileKind, FileSystem, ReadTextLinesOptions, RemoveOptions, Shell, ShellExecOptions,
            ShellExecResult, WriteContent,
        },
        utils::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
    },
    types::AgentTool,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

/// Hard cap on captured command output, per stream; the bash tool applies
/// its own line/byte truncation on top, so this only guards our memory.
const EXEC_OUTPUT_CAP: u64 = 2 * 1024 * 1024;

/// The app identity web tools send as their User-Agent — one constant
/// across fetch and the search adapters.
pub(crate) const USER_AGENT: &str = concat!("holt/", env!("CARGO_PKG_VERSION"));

/// Trim `text` to at most `max_bytes`, never splitting a character:
/// the byte-envelope helper both web tools clamp their model-facing
/// output through. Returns the trimmed text and whether it was cut.
pub(crate) fn clamp_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The host filesystem and shell, rooted at the chat's working directory.
/// Blocking std filesystem calls run inline — the holt-core tools only issue
/// small reads/writes, and `exec` (the long-running case) is fully async.
/// Content search, the one long-running local operation that is not an env
/// method, runs its walk on `spawn_blocking` instead (see [`grep`]).
pub(crate) struct LocalExecutionEnv {
    cwd: String,
    /// The login shell resolved once per environment; every agent command
    /// runs under it (see [`Shell::exec`]).
    shell: String,
    /// Where each running command's process group is recorded, so launch
    /// can kill one a hard kill orphaned (see [`crate::shell_groups`]).
    /// `None` records nothing.
    shell_groups: Option<std::path::PathBuf>,
}

impl LocalExecutionEnv {
    pub(crate) fn new(cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        Self {
            // The env anchors every addressed path; canonicalize the anchor
            // itself so a symlinked cwd (macOS /tmp, a user-mounted alias)
            // doesn't make every write "traverse a symlink" under
            // `write_to_file`'s landing check. An unresolvable cwd (empty,
            // not yet created) keeps its spelling.
            cwd: Path::new(&cwd)
                .canonicalize()
                .ok()
                .and_then(|path| path.to_str().map(str::to_owned))
                .unwrap_or(cwd),
            shell: login_shell(),
            shell_groups: None,
        }
    }

    #[cfg(test)]
    fn with_shell(cwd: impl Into<String>, shell: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            shell: shell.into(),
            shell_groups: None,
        }
    }
}

fn io_file_error(error: &std::io::Error, path: &str) -> FileError {
    use std::io::ErrorKind::*;
    let code = match error.kind() {
        NotFound => FileErrorCode::NotFound,
        PermissionDenied => FileErrorCode::PermissionDenied,
        _ => FileErrorCode::Unknown,
    };
    FileError::with_path(code, error.to_string(), path)
}

/// Lexically normalize an absolute path (`.`/`..` collapsed, no symlink
/// resolution, trailing slash stripped) — the "addressed path" shape the
/// contract asks for.
fn normalize_absolute(path: &str) -> String {
    debug_assert!(path.starts_with('/'));
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut out = String::with_capacity(path.len());
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Resolve a tool-call path argument against `cwd` (the env's "addressed
/// path" shape: lexical, no symlink resolution). Also used engine-side to
/// match read targets against the skill catalog.
pub(crate) fn to_absolute(cwd: &str, path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        path.to_string()
    } else {
        format!("{cwd}/{path}")
    };
    normalize_absolute(&joined)
}

fn path_kind(metadata: &std::fs::Metadata) -> FileKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    }
}

fn entry_info(path: &Path, metadata: std::fs::Metadata) -> FileInfo {
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0);
    FileInfo {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.to_string_lossy().into_owned(),
        kind: path_kind(&metadata),
        size: metadata.len(),
        mtime_ms,
    }
}

/// Where `path` will actually land: canonicalize the deepest existing
/// ancestor and re-attach the not-yet-existing tail. Differs from the
/// addressed (lexical) spelling exactly when some existing component —
/// leaf or intermediate — is a symlink.
fn resolve_landing(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = path;
    loop {
        match cursor.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match (cursor.file_name(), cursor.parent()) {
                    (Some(name), Some(parent)) => {
                        missing.push(name);
                        cursor = parent;
                    }
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_to_file(
    root: &Path,
    path: &str,
    content: &WriteContent,
    append: bool,
) -> Result<(), FileError> {
    let addressed = Path::new(path);
    // The approval and any always-allow grant bind to the addressed
    // spelling, so the bytes must land on exactly that entry: a path
    // under the workspace that traverses a symlink (leaf or
    // intermediate) is refused rather than followed to a file the gate
    // never named — the repo-shipped-symlink case (ADR-permission-modes-gate-mutations-no-os-sandbox; audit
    // run-3 finding `write-grant-symlink-landing`). The error names the
    // resolved target — the model may address it directly, and the user
    // then approves the real path. Paths outside the workspace are
    // exempt: they arrive already absolute (engine temp files under
    // macOS's symlinked /var, dotfile setups that symlink ~/.zshrc), so
    // their spelling carries no workspace entry to impersonate.
    let confined = addressed.starts_with(root);
    if confined {
        let landing = resolve_landing(addressed).map_err(|error| io_file_error(&error, path))?;
        if landing != addressed {
            return Err(FileError::with_path(
                FileErrorCode::PermissionDenied,
                format!(
                    "{path} traverses a symlink; refusing to write through it. \
                     Address the resolved path {} directly if that target is intended.",
                    landing.display()
                ),
                path,
            ));
        }
    }
    if let Some(parent) = addressed.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_file_error(&error, &format!("{}/", parent.display())))?;
    }
    let bytes = match content {
        WriteContent::Text(text) => text.as_bytes(),
        WriteContent::Bytes(bytes) => bytes.as_slice(),
    };
    if !append {
        // A whole-file write replaces atomically: a crash mid-write must
        // not leave a half-written file for a resumed Turn to build on
        // (ADR-turns-resume-from-an-intent-journal). A confined path was
        // proven symlink-free above, and the rename replaces the entry
        // itself; an outside path keeps writing through its symlink, as
        // the open did.
        let target = if confined {
            addressed.to_path_buf()
        } else {
            std::fs::canonicalize(addressed).unwrap_or_else(|_| addressed.to_path_buf())
        };
        return replace_atomically(&target, bytes).map_err(|error| io_file_error(&error, path));
    }
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append);
    // TOCTOU backstop for a confined leaf: never open through a symlink
    // swapped in after the landing check above.
    #[cfg(unix)]
    if confined {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return FileError::with_path(
                FileErrorCode::PermissionDenied,
                format!("{path} is a symlink; refusing to write through it"),
                path,
            );
        }
        io_file_error(&error, path)
    })?;
    std::io::Write::write_all(&mut file, bytes).map_err(|error| io_file_error(&error, path))
}

/// Replace `path` with `bytes` through a synced sibling temp file and a
/// rename, keeping the existing file's permissions.
fn replace_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = parent.join(format!(".{name}.holt-write-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, meta.permissions())?;
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

impl FileSystem for LocalExecutionEnv {
    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move { Ok(to_absolute(&self.cwd, path)) })
    }

    fn join_path<'a>(
        &'a self,
        parts: &'a [String],
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let joined = parts.join("/");
            Ok(to_absolute(&self.cwd, &joined))
        })
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            std::fs::read_to_string(&path).map_err(|error| io_file_error(&error, &path))
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a str,
        options: ReadTextLinesOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<String>, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            let text =
                std::fs::read_to_string(&path).map_err(|error| io_file_error(&error, &path))?;
            let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
            if let Some(max_lines) = options.max_lines {
                lines.truncate(max_lines);
            }
            Ok(lines)
        })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            // Directories have no byte content: name the situation and point
            // at `ls` instead of leaking the image codec's not-a-file error
            // (the read tool reads everything through here).
            if std::fs::metadata(&path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
            {
                return Err(FileError::with_path(
                    FileErrorCode::IsDirectory,
                    format!(
                        "Is a directory: {path}. Directories cannot be read; \
use the ls tool to list a directory's entries."
                    ),
                    &path,
                ));
            }
            let error_path = path.clone();
            let read = tokio::task::spawn_blocking(move || {
                crate::images::codec::read_for_tool(Path::new(&path))
            });
            tokio::select! {
                result = read => result.map_err(|e| FileError::with_path(FileErrorCode::Unknown, e.to_string(), &error_path))?
                    .map_err(|e| FileError::with_path(FileErrorCode::Unknown, e, &error_path)),
                _ = async { match abort_signal { Some(signal) => signal.cancelled().await, None => pending().await } } =>
                    Err(FileError::with_path(FileErrorCode::Unknown, "Read interrupted", &error_path)),
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            write_to_file(Path::new(&self.cwd), &path, content, false)
        })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            write_to_file(Path::new(&self.cwd), &path, content, true)
        })
    }

    fn rename_file<'a>(
        &'a self,
        source_path: &'a str,
        destination_path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let source_path = to_absolute(&self.cwd, source_path);
            let destination_path = to_absolute(&self.cwd, destination_path);
            if let Some(parent) = Path::new(destination_path.as_str()).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| io_file_error(&error, &format!("{}/", parent.display())))?;
            }
            std::fs::rename(&source_path, &destination_path)
                .map_err(|error| io_file_error(&error, &source_path))
        })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<FileInfo, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            std::fs::symlink_metadata(&path)
                .map(|metadata| entry_info(Path::new(&path), metadata))
                .map_err(|error| io_file_error(&error, &path))
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            let mut entries = Vec::new();
            let reader = std::fs::read_dir(&path).map_err(|error| io_file_error(&error, &path))?;
            for entry in reader {
                let entry = entry.map_err(|error| io_file_error(&error, &path))?;
                // Broken symlinks list as entries with no metadata; skip them
                // rather than failing the whole listing.
                if let Ok(metadata) = entry.metadata() {
                    entries.push(entry_info(&entry.path(), metadata));
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
    }

    fn canonical_path<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            std::fs::canonicalize(&path)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|error| io_file_error(&error, &path))
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<bool, FileError>> {
        Box::pin(async move {
            Ok(Path::new(&to_absolute(&self.cwd, path))
                .symlink_metadata()
                .is_ok())
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a str,
        options: CreateDirOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let result = if options.recursive.unwrap_or(true) {
                std::fs::create_dir_all(path)
            } else {
                std::fs::create_dir(path)
            };
            result.map_err(|error| io_file_error(&error, path))
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a str,
        options: RemoveOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let metadata = match Path::new(path).symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if options.force.unwrap_or(false) {
                        return Ok(());
                    }
                    return Err(io_file_error(&error, path));
                }
                Err(error) => return Err(io_file_error(&error, path)),
            };
            let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                if options.recursive.unwrap_or(false) {
                    std::fs::remove_dir_all(path)
                } else {
                    std::fs::remove_dir(path)
                }
            } else {
                std::fs::remove_file(path)
            };
            result.map_err(|error| io_file_error(&error, path))
        })
    }

    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir)
                .map(|_| dir.to_string_lossy().into_owned())
                .map_err(|error| io_file_error(&error, &dir.to_string_lossy()))
        })
    }

    fn create_temp_file<'a>(
        &'a self,
        options: &'a CreateTempFileOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let name = format!(
                "{}tmp-{}{}",
                options.prefix.as_deref().unwrap_or(""),
                uuid::Uuid::new_v4(),
                options.suffix.as_deref().unwrap_or("")
            );
            let path = std::env::temp_dir().join(name);
            std::fs::File::create(&path)
                .map(|_| path.to_string_lossy().into_owned())
                .map_err(|error| io_file_error(&error, &path.to_string_lossy()))
        })
    }

    fn cleanup(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

fn io_execution_error(error: &std::io::Error) -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string())
}

#[cfg(unix)]
struct ShellProcessGroup {
    id: Option<u32>,
    record: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl ShellProcessGroup {
    fn signal(&self, signal: libc::c_int) -> bool {
        let Some(id) = self.id else { return false };
        // SAFETY: the child was spawned with process_group(0), so its PID
        // identifies only this execution's process group.
        if unsafe { libc::kill(-(id as libc::pid_t), signal) } == 0 {
            return true;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pid = id, %error, "could not signal shell process group");
        }
        false
    }
}

#[cfg(unix)]
impl Drop for ShellProcessGroup {
    fn drop(&mut self) {
        // Also covers the execution future being dropped during cleanup.
        self.signal(libc::SIGKILL);
        if let Some(record) = &self.record {
            crate::shell_groups::forget(record);
        }
    }
}

impl Shell for LocalExecutionEnv {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        options: Option<&'a ShellExecOptions>,
    ) -> BoxFuture<'a, Result<ShellExecResult, ExecutionError>> {
        Box::pin(async move {
            let cwd = options
                .and_then(|options| options.cwd.clone())
                .unwrap_or_else(|| self.cwd.clone());
            // `-l` sources the user's login profiles — the shell owns PATH and
            // the rest of its environment, so no probe or override here (see
            // `shell_env`). `-c` keeps the session non-interactive: no prompt,
            // no stdin reads, exit code straight from the command.
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-lc")
                .arg(command)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
            match options.and_then(|options| options.inherit_env) {
                Some(false) => {
                    cmd.env_clear();
                    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
                    if let Some(env) = options.and_then(|options| options.env.as_ref()) {
                        cmd.envs(env);
                    }
                }
                _ => {
                    if let Some(env) = options.and_then(|options| options.env.as_ref()) {
                        cmd.envs(env);
                    }
                }
            }
            let mut child = cmd.spawn().map_err(|error| io_execution_error(&error))?;
            #[cfg(unix)]
            let mut group = ShellProcessGroup {
                id: child.id(),
                record: self
                    .shell_groups
                    .as_deref()
                    .zip(child.id())
                    .and_then(|(dir, id)| crate::shell_groups::record(dir, id)),
            };
            // Readers own their pipes; capped so `cat huge.log` cannot balloon
            // memory ahead of the bash tool's own truncation.
            async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut pipe: Option<R>) -> String {
                let mut buffer = Vec::new();
                if let Some(pipe) = pipe.as_mut() {
                    let _ = pipe.take(EXEC_OUTPUT_CAP).read_to_end(&mut buffer).await;
                }
                String::from_utf8_lossy(&buffer).into_owned()
            }
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let aborted = options.and_then(|options| options.abort_signal.clone());
            let timeout = options
                .and_then(|options| options.timeout)
                .map(Duration::from_secs_f64);
            let result = {
                // Output pipes can outlive the shell when descendants inherit
                // them. Keep the entire wait cancellable, with no detached readers.
                let execution = async {
                    let (status, stdout, stderr) =
                        tokio::join!(child.wait(), read_capped(stdout), read_capped(stderr),);
                    status
                        .map(|status| (status, stdout, stderr))
                        .map_err(|error| io_execution_error(&error))
                };
                tokio::select! {
                    biased;
                    _ = async {
                        match aborted.as_ref() {
                            Some(signal) => signal.cancelled().await,
                            None => pending::<()>().await,
                        }
                    } => Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "command aborted",
                    )),
                    _ = async {
                        match timeout {
                            Some(budget) => sleep(budget).await,
                            None => pending::<()>().await,
                        }
                    } => Err(ExecutionError::new(
                        ExecutionErrorCode::Timeout,
                        format!("command exceeded its timeout: {command}"),
                    )),
                    result = execution => result,
                }
            };
            let (status, stdout, stderr) = match result {
                Ok(result) => {
                    #[cfg(unix)]
                    {
                        group.id = None;
                    }
                    result
                }
                Err(error) => {
                    #[cfg(unix)]
                    {
                        if group.signal(libc::SIGTERM) {
                            sleep(Duration::from_millis(250)).await;
                        }
                        group.signal(libc::SIGKILL);
                        group.id = None;
                    }
                    // Reap the direct child even if the shell already exited
                    // and it was only a descendant's output pipe holding us up.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(error);
                }
            };
            if let Some(options) = options {
                if let Some(listener) = options.on_stdout.as_ref()
                    && let Err(_error) = listener(&stdout)
                {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::CallbackError,
                        "stdout listener failed",
                    ));
                }
                if let Some(listener) = options.on_stderr.as_ref()
                    && listener(&stderr).is_err()
                {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::CallbackError,
                        "stderr listener failed",
                    ));
                }
            }
            Ok(ShellExecResult {
                stdout,
                stderr,
                exit_code: status.code().unwrap_or(-1),
            })
        })
    }

    fn cleanup(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Rebuild a harness tool as a plain [`AgentTool`] whose `execute` injects
/// the shared [`ExecutionToolContext`] — what
/// `AgentHarnessTool::to_agent_tool` does, but with a working context
/// instead of an empty one (the built-ins reject calls without it).
fn with_execution_context(tool: AgentHarnessTool, context: &AgentToolContext) -> AgentTool {
    let execute = Arc::clone(&tool.execute);
    let context = Arc::clone(context);
    AgentTool {
        name: tool.name,
        label: tool.label,
        description: tool.description,
        parameters: tool.parameters,
        constrained_sampling: tool.constrained_sampling,
        prepare_arguments: tool.prepare_arguments,
        execution_mode: tool.execution_mode,
        execute: Arc::new(move |tool_call_id, params, signal, on_update| {
            (execute)(tool_call_id, params, signal, on_update, &context)
        }),
    }
}

/// The toolset handed to the agent loop: holt-core's built-in read/write/
/// edit/bash, all running against one environment rooted at `cwd`, plus
/// holt's own content-search and web-fetch tools, exposed to the agent as
/// `grep` and `web_fetch` — and `web_search` when a backend is configured
/// (no backend means the tool is absent, never registered-and-erroring).
#[cfg(test)]
pub(crate) fn execution_tools(cwd: &str) -> Vec<AgentTool> {
    execution_tools_for_model(cwd, true, None, None, None)
}

/// The per-Turn write-attribution handle (ADR-turn-change-sets-and-review): the tools record the
/// paths they mutate; the Turn change set keeps only attributed files, so
/// another chat's concurrent work in the same working tree never lands in
/// this Turn's card. Cloned per tool; the set is shared. `None` on runs
/// without a Turn (compaction) — nothing records, nothing filters.
///
/// With a step log, each call that changed files also records a step
/// checkpoint (ADR-step-checkpoints-in-a-chat-owned-object-store).
#[derive(Clone)]
pub(crate) struct ChangeAttribution {
    set: Arc<Attribution>,
    git: Git,
    steps: Option<crate::checkpoints::StepLog>,
}

impl ChangeAttribution {
    pub(crate) fn new(
        set: Arc<Attribution>,
        git: Git,
        steps: Option<crate::checkpoints::StepLog>,
    ) -> Self {
        Self { set, git, steps }
    }

    /// A write/edit target's side before the call, when the Turn
    /// checkpoints, kept on disk while the call runs. Best-effort: a failed
    /// read records no step.
    async fn before_write(
        &self,
        cwd: &str,
        path: &str,
        call: (&str, &str),
    ) -> Option<(String, CheckpointSide, Option<RunningCall>)> {
        let steps = self.steps.as_ref()?;
        let (relative, before) = self
            .git
            .path_checkpoint(cwd, steps.store(), path)
            .await
            .ok()
            .flatten()?;
        let running = steps.begin(
            call.0,
            call.1,
            cwd,
            CallStart::Path {
                path: relative.clone(),
                before: before.clone(),
            },
        );
        Some((relative, before, running))
    }

    /// Keep a bash window's start on disk while the command runs.
    fn begin_window(
        &self,
        cwd: &str,
        call: (&str, &str),
        window: &WindowStart,
    ) -> Option<RunningCall> {
        let start = window.start.clone()?;
        self.steps.as_ref()?.begin(call.0, call.1, cwd, start)
    }

    /// A write/edit target's path, resolved to the repo-root-relative
    /// form the capture vocabulary uses, and — with its before side — its
    /// step. Recording is best-effort: an unresolved path attributes
    /// nothing, never fails the tool.
    async fn record_tool_path(
        &self,
        cwd: &str,
        path: &str,
        call: (&str, &str),
        before: Option<(String, CheckpointSide, Option<RunningCall>)>,
    ) {
        if let Some(relative) = self.git.repo_relative(cwd, path).await {
            self.set.record([relative]);
        }
        let (Some(steps), Some((relative, before, _running))) = (&self.steps, before) else {
            return;
        };
        if let Ok(Some((_, after))) = self.git.path_checkpoint(cwd, steps.store(), path).await
            && after != before
        {
            steps.append(
                call.0,
                call.1,
                vec![CheckpointFile {
                    path: relative,
                    before,
                    after,
                }],
            );
        }
    }

    /// A bash command's window start: the current uncommitted section map,
    /// plus the before sides a step checkpoint needs.
    async fn window_start(&self, cwd: &str) -> WindowStart {
        self.git
            .window_start(cwd, self.steps.as_ref().map(|steps| steps.store()))
            .await
    }

    /// Attribute a bash command's writes: every path whose section changed
    /// across the command's window (creations, modifications, deletions).
    /// Runs after the command whatever its exit code — a failed command's
    /// partial writes still happened.
    async fn record_bash_delta(&self, cwd: &str, call: (&str, &str), before: WindowStart) {
        let after = self.git.changed_sections(cwd).await;
        let touched = after
            .iter()
            .filter(|(path, text)| before.sections.get(*path) != Some(*text))
            .map(|(path, _)| path.clone())
            .chain(
                before
                    .sections
                    .keys()
                    .filter(|path| !after.contains_key(*path))
                    .cloned(),
            )
            .collect::<Vec<_>>();
        self.set.record(touched.iter().cloned());
        let Some(steps) = &self.steps else {
            return;
        };
        if touched.is_empty() {
            return;
        }
        match self
            .git
            .window_checkpoint(cwd, steps.store(), before, touched)
            .await
        {
            Ok(Some(files)) => steps.append(call.0, call.1, files),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(target: "holt::checkpoints", %error, "could not store a step checkpoint");
            }
        }
    }
}

/// Wrap a write/edit tool so a successful call records its target path.
fn attribute_tool(
    mut tool: AgentTool,
    attribution: Option<&ChangeAttribution>,
    cwd: &str,
) -> AgentTool {
    let Some(attribution) = attribution else {
        return tool;
    };
    let attribution = attribution.clone();
    let cwd = cwd.to_string();
    let name = tool.name.clone();
    let execute = Arc::clone(&tool.execute);
    tool.execute = Arc::new(move |id, params, signal, update| {
        let attribution = attribution.clone();
        let cwd = cwd.clone();
        let name = name.clone();
        let call = id.to_string();
        let path = params
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let future = execute(id, params, signal, update);
        Box::pin(async move {
            let before = match path.as_deref() {
                Some(path) => attribution.before_write(&cwd, path, (&call, &name)).await,
                None => None,
            };
            let result = future.await;
            if result.is_ok()
                && let Some(path) = path.as_deref()
            {
                attribution
                    .record_tool_path(&cwd, path, (&call, &name), before)
                    .await;
            }
            result
        })
    });
    tool
}

/// Wrap the bash tool so the command's window records its write delta.
fn attribute_bash(
    mut tool: AgentTool,
    attribution: Option<&ChangeAttribution>,
    cwd: &str,
) -> AgentTool {
    let Some(attribution) = attribution else {
        return tool;
    };
    let attribution = attribution.clone();
    let cwd = cwd.to_string();
    let name = tool.name.clone();
    let execute = Arc::clone(&tool.execute);
    tool.execute = Arc::new(move |id, params, signal, update| {
        let attribution = attribution.clone();
        let cwd = cwd.clone();
        let name = name.clone();
        let call = id.to_string();
        let future = execute(id, params, signal, update);
        Box::pin(async move {
            let before = attribution.window_start(&cwd).await;
            let _running = attribution.begin_window(&cwd, (&call, &name), &before);
            let result = future.await;
            attribution
                .record_bash_delta(&cwd, (&call, &name), before)
                .await;
            result
        })
    });
    tool
}

/// The Turn's toolset. With `shell_groups`, the shell records each
/// command's process group there while it runs (see [`crate::shell_groups`]).
pub(crate) fn execution_tools_for_model(
    cwd: &str,
    allow_images: bool,
    search_backend: Option<Arc<dyn web_search::SearchBackend>>,
    attribution: Option<ChangeAttribution>,
    shell_groups: Option<std::path::PathBuf>,
) -> Vec<AgentTool> {
    let mut env = LocalExecutionEnv::new(cwd);
    env.shell_groups = shell_groups;
    let env: Arc<dyn ExecutionEnv> = Arc::new(env);
    let context = ExecutionToolContext { env }.into_tool_context();
    let mut tools = vec![
        image_read_tool(&context, allow_images),
        attribute_tool(
            with_execution_context(create_write_tool(), &context),
            attribution.as_ref(),
            cwd,
        ),
        attribute_tool(
            with_execution_context(create_edit_tool(), &context),
            attribution.as_ref(),
            cwd,
        ),
        attribute_bash(bash_tool(&context), attribution.as_ref(), cwd),
        grep::create_grep_tool(cwd),
        ls::create_ls_tool(cwd),
        web_fetch::create_web_fetch_tool(),
        ask_user::create_ask_user_tool(),
    ];
    if let Some(backend) = search_backend {
        tools.push(web_search::create_web_search_tool(backend));
    }
    tools
}

pub(crate) use model_setup::create_choose_provider_tool;
pub(crate) use model_setup::create_model_proposal_tool;
pub(crate) use model_setup::create_request_provider_key_tool;
pub(crate) use read_chat::create_read_chat_tool;
pub(crate) use todo::create_todo_tool;
pub use web_search::{SearchBackend, SearchHit, SearchResults};

/// Foreground commands get a bounded wait. The engine applies no timeout of
/// its own (holt-core leaves it optional by design), so a runaway command — a
/// recursive scan over `target/`, a server process inheriting the output
/// pipe — would otherwise hang the tool until the user aborts.
const DEFAULT_BASH_TIMEOUT_SECONDS: f64 = 300.0;

/// Injected at the params level, not the exec layer, so holt-core's timeout
/// validation and its "timed out after N seconds" message report the
/// effective value.
fn apply_default_bash_timeout(params: &mut serde_json::Value) {
    if let Some(object) = params.as_object_mut()
        && !object.contains_key("timeout")
    {
        object.insert("timeout".into(), DEFAULT_BASH_TIMEOUT_SECONDS.into());
    }
}

/// The holt-core bash tool, retargeted to the user's login shell. The harness
/// fixes the tool *name* as `bash`; the executable choice is holt's (see
/// [`LocalExecutionEnv::exec`]) and so is the description — the user's shell
/// may be zsh or anything else, so the model must not assume bash syntax.
fn bash_tool(context: &AgentToolContext) -> AgentTool {
    let mut tool = with_execution_context(create_bash_tool(BashToolOptions::default()), context);
    let shell = login_shell();
    tool.description = format!(
        "Execute a command in the user's login shell ({shell}). It runs as a non-interactive login shell in the current working directory, so the user's profile environment (PATH, toolchains) is loaded. Returns stdout and stderr. A non-zero exit code is reported as a failed call, including a plain `grep` that matches nothing (exit 1) — the `grep` tool returns 'No matches found.' as an ordinary result instead. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds; when omitted, a {}-second default applies.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024,
        DEFAULT_BASH_TIMEOUT_SECONDS,
    );
    if shell.ends_with("zsh") {
        tool.description.push_str(
            " This shell is zsh, not bash: quote glob patterns (`--include=\"*.rs\"`) — an unquoted pattern that matches nothing aborts the command — and never pass a bare `=word` argument, which zsh expands as a command path.",
        );
    }
    tool.parameters["properties"]["command"]["description"] =
        "Command to execute in the user's login shell".into();
    tool.parameters["properties"]["timeout"]["description"] =
        format!("Timeout in seconds (default {DEFAULT_BASH_TIMEOUT_SECONDS} when omitted)").into();
    let execute = Arc::clone(&tool.execute);
    tool.execute = Arc::new(move |id, params, signal, update| {
        let mut params = params.clone();
        apply_default_bash_timeout(&mut params);
        execute(id, &params, signal, update)
    });
    tool
}

fn image_read_tool(context: &AgentToolContext, allow_images: bool) -> AgentTool {
    use base64::Engine as _;
    let mut tool = with_execution_context(create_read_tool(ReadToolOptions::default()), context);
    tool.description = "Read text files or images at a local path. Text supports offset/limit. Images: static PNG/JPEG and first-frame GIF/WebP, up to 25 MiB and 64 megapixels. Model input is proportionally resized to at most 2048 pixels per edge and 5 MiB PNG; source files are unchanged. Image input requires a visual model. Directories cannot be read — use `ls` to list a directory's entries.".into();
    let context = context.clone();
    tool.execute = Arc::new(move |id, params, signal, update| {
        let context = context.clone();
        let id = id.to_string();
        let params = params.clone();
        let signal = signal.cloned();
        let update = update.cloned();
        Box::pin(async move {
            // The upstream hook has no error flag; retain its failure separately
            // for this invocation, including concurrent read calls.
            let failure = Arc::new(std::sync::Mutex::new(None));
            let hook_failure = failure.clone();
            let processor = Arc::new(move |bytes: &[u8], _: &str, _: bool| {
                let bytes = bytes.to_vec();
                let failure = hook_failure.clone();
                Box::pin(async move {
                    let processed = if allow_images {
                        let permit = crate::images::PROCESSING
                            .clone()
                            .acquire_owned()
                            .await
                            .unwrap();
                        tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            crate::images::codec::model_image(&bytes)
                        })
                        .await
                        .unwrap_or_else(|error| Err(format!("Image processing failed: {error}")))
                    } else {
                        Err("The selected model does not support image input. No pixels were supplied. Text files can still be read.".into())
                    };
                    match processed {
                        Ok((bytes, hints)) => ReadImageProcessorResult::Ok {
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                            mime_type: "image/png".into(),
                            hints,
                        },
                        Err(error) => {
                            *failure.lock().unwrap() = Some(error.clone());
                            ReadImageProcessorResult::Err(error)
                        }
                    }
                }) as BoxFuture<'static, ReadImageProcessorResult>
            });
            let read = with_execution_context(
                create_read_tool(ReadToolOptions {
                    auto_resize_images: Some(true),
                    image_processor: Some(processor),
                    max_lines: None,
                    max_bytes: None,
                }),
                &context,
            );
            let result = tokio::select! {
                result = (read.execute)(&id, &params, signal.as_ref(), update.as_ref()) => result?,
                _ = async { match signal.as_ref() { Some(signal) => signal.cancelled().await, None => pending().await } } => return Err("Image read interrupted".into()),
            };
            if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err("Image read interrupted".into());
            }
            if let Some(error) = failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(result)
        })
    });
    tool
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        (path, dir)
    }

    #[tokio::test]
    async fn reads_and_writes_relative_to_cwd() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let path = env.absolute_path("notes/a.txt", None).await.unwrap();
        env.write_file(&path, &"hello".into(), None).await.unwrap();
        assert_eq!(env.read_text_file(&path, None).await.unwrap(), "hello");
        assert_eq!(
            env.read_text_file("notes/a.txt", None).await.unwrap(),
            "hello"
        );
        assert!(env.exists("notes/a.txt", None).await.unwrap());
        let listing = env.list_dir("notes", None).await.unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "a.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_refuses_a_leaf_symlink_and_leaves_the_target_untouched() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let root_path = std::path::Path::new(&root);
        let target = root_path.join("target.txt");
        std::fs::write(&target, "original").unwrap();
        // A tracked symlink shipped by an untrusted repo: the addressed
        // path names the link, the bytes would land on the target.
        std::os::unix::fs::symlink(&target, root_path.join("notes.txt")).unwrap();
        let error = env
            .write_file("notes.txt", &"payload".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, FileErrorCode::PermissionDenied);
        assert!(
            error.message.contains("symlink"),
            "unexpected: {}",
            error.message
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert!(
            root_path
                .join("notes.txt")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_refuses_a_symlinked_intermediate_directory() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), std::path::Path::new(&root).join("assets"))
            .unwrap();
        let error = env
            .write_file("assets/newdir/file.txt", &"payload".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, FileErrorCode::PermissionDenied);
        // create_dir_all must not have followed the link either.
        assert!(!outside.path().join("newdir").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_lands_when_addressed_by_the_resolved_path() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let target = std::path::Path::new(&root).join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let resolved = target.canonicalize().unwrap();
        env.write_file(resolved.to_str().unwrap(), &"updated".into(), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_outside_the_workspace_may_follow_a_symlink() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("real.txt");
        std::fs::write(&target, "original").unwrap();
        let link = outside.path().join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // Absolute paths outside the workspace arrive already spelled
        // (engine temp files under macOS's symlinked /var, symlinked
        // dotfiles) — the landing check does not confine them.
        env.write_file(link.to_str().unwrap(), &"updated".into(), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
        // The atomic replace lands on the resolved file, never over the link.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_whole_file_write_replaces_atomically_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let root_path = std::path::Path::new(&root);
        let target = root_path.join("run.sh");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = std::fs::metadata(&target).unwrap();
        env.write_file("run.sh", &"new".into(), None).await.unwrap();
        let after = std::fs::metadata(&target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(after.permissions().mode() & 0o777, 0o755);
        // A new inode: the old one was replaced whole, never truncated.
        assert_ne!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after)
        );
        let leftovers: Vec<_> = std::fs::read_dir(root_path)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("holt-write"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn reading_a_directory_names_it_and_points_at_ls() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        std::fs::create_dir(std::path::Path::new(&root).join("sub")).unwrap();
        let error = env.read_binary_file("sub", None).await.unwrap_err();
        assert_eq!(error.code, FileErrorCode::IsDirectory);
        assert!(
            error.message.contains("Is a directory") && error.message.contains("ls tool"),
            "unexpected: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn exec_returns_output_and_exit_code() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let result = env.exec("printf 'hi'", None).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hi");
        let failed = env.exec("exit 3", None).await.unwrap();
        assert_eq!(failed.exit_code, 3);
    }

    #[tokio::test]
    async fn exec_times_out_and_aborts() {
        let (root, _guard) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let options = ShellExecOptions {
            timeout: Some(0.05),
            ..Default::default()
        };
        let error = env.exec("sleep 5", Some(&options)).await.unwrap_err();
        assert_eq!(error.code, ExecutionErrorCode::Timeout);
        let signal = CancellationToken::new();
        let options = ShellExecOptions {
            abort_signal: Some(signal.clone()),
            ..Default::default()
        };
        let task = tokio::spawn(async move { env.exec("sleep 5", Some(&options)).await });
        signal.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.code, ExecutionErrorCode::Aborted);
    }

    #[test]
    fn bash_timeout_default_only_fills_missing() {
        let mut params = serde_json::json!({ "command": "ls" });
        apply_default_bash_timeout(&mut params);
        assert_eq!(params["timeout"], 300.0);

        let mut params = serde_json::json!({ "command": "ls", "timeout": 900 });
        apply_default_bash_timeout(&mut params);
        assert_eq!(params["timeout"], 900);

        // A non-object payload must not panic; it fails schema validation
        // downstream, not here.
        let mut params = serde_json::json!(null);
        apply_default_bash_timeout(&mut params);
        assert!(params.get("timeout").is_none());
    }

    #[tokio::test]
    async fn bash_tool_wraps_execute_without_disturbing_fast_calls() {
        let tools = execution_tools_for_model(".", true, None, None, None);
        let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
        let result = (bash.execute)(
            "test-call",
            &serde_json::json!({ "command": "true" }),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.content.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_runs_commands_through_the_login_shell() {
        use std::os::unix::fs::PermissionsExt;

        let (root, _guard) = temp_root();
        let dir = tempfile::tempdir().unwrap();
        // Stands in for a toolchain the user's profile puts on PATH.
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(&cargo, "#!/bin/sh\necho fake-cargo 1.0\n").unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A stand-in login shell: its "profiles" set a variable and a PATH
        // entry, then the command runs — what zsh/bash do after their startup
        // files. It refuses anything but the `-lc` invocation exec must use.
        let shell = dir.path().join("login-shell");
        std::fs::write(
            &shell,
                format!(
                    "#!/bin/sh\n[ \"$1\" = -lc ] || exit 97\nHOLT_PROFILE_VAR=from-profile\nexport HOLT_PROFILE_VAR\nPATH=\"{bin}:$PATH\"\nexport PATH\nexec /bin/sh -c \"$2\"\n",
                    bin = bin.display()
                ),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();

        let env = LocalExecutionEnv::with_shell(&root, shell.to_str().unwrap());
        // A minimal parent environment: `cargo` must be found via the PATH the
        // shell's profile builds, and the profile variable must be visible.
        let options = ShellExecOptions {
            inherit_env: Some(false),
            ..Default::default()
        };
        let result = env
            .exec(
                "cargo --version; printf ' %s' \"$HOLT_PROFILE_VAR\"",
                Some(&options),
            )
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        assert!(
            result.stdout.contains("fake-cargo 1.0"),
            "unexpected output: {}",
            result.stdout
        );
        assert!(
            result.stdout.contains("from-profile"),
            "unexpected output: {}",
            result.stdout
        );
    }

    #[cfg(unix)]
    struct TestChild(i32);

    #[cfg(unix)]
    impl Drop for TestChild {
        fn drop(&mut self) {
            // SAFETY: this PID belongs to the child created by this test.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    #[cfg(unix)]
    async fn shell_child_ready(root: &str) -> TestChild {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(Path::new(root).join("child.pid"))
                    && let Ok(pid) = pid.trim().parse::<i32>()
                    && Path::new(root).join("writes").exists()
                {
                    return TestChild(pid);
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell child did not start")
    }

    #[cfg(unix)]
    const WRITING_CHILD: &str = r#"bash -c 'trap "" TERM; echo $$ > child.pid; for ((i=0; i<500; i++)); do echo tick >> writes; sleep 0.01; done'"#;

    #[cfg(unix)]
    async fn assert_descendant_stopped_writing(root: &str) {
        let path = Path::new(root).join("writes");
        let writes = std::fs::read(&path).unwrap();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            std::fs::read(&path).unwrap(),
            writes,
            "descendant kept writing after exec returned"
        );
    }

    #[cfg(unix)]
    async fn shell_leader_reaped(root: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(Path::new(root).join("leader.pid"))
                    && let Ok(pid) = pid.trim().parse::<i32>()
                    // SAFETY: signal 0 only checks existence of the test's process.
                    && unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell leader did not exit");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_abort_stops_descendant_writes_before_returning() {
        let (root, _dir) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let signal = CancellationToken::new();
        let options = ShellExecOptions {
            abort_signal: Some(signal.clone()),
            ..Default::default()
        };
        let command = format!("{WRITING_CHILD} & wait");
        let mut execution = env.exec(&command, Some(&options));
        let _child = tokio::select! {
            result = &mut execution => panic!("command ended before cancellation: {result:?}"),
            child = shell_child_ready(&root) => child,
        };
        signal.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), &mut execution).await;
        assert_eq!(
            result.unwrap().unwrap_err().code,
            ExecutionErrorCode::Aborted
        );
        assert_descendant_stopped_writing(&root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_output_wait_remains_interruptible_after_the_shell_exits() {
        for cancel in [true, false] {
            let (root, _dir) = temp_root();
            let env = LocalExecutionEnv::new(&root);
            let signal = CancellationToken::new();
            let options = ShellExecOptions {
                abort_signal: Some(signal.clone()),
                timeout: (!cancel).then_some(1.0),
                ..Default::default()
            };
            let command = format!("echo $$ > leader.pid; {WRITING_CHILD} &");
            let mut execution = env.exec(&command, Some(&options));
            let _child = tokio::select! {
                result = &mut execution => panic!("command ended before the shell exited: {result:?}"),
                child = async {
                    let child = shell_child_ready(&root).await;
                    shell_leader_reaped(&root).await;
                    child
                } => child,
            };
            if cancel {
                signal.cancel();
            }
            let error = tokio::time::timeout(Duration::from_secs(3), &mut execution)
                .await
                .expect("output wait ignored cancellation or timeout")
                .unwrap_err();
            assert_eq!(
                error.code,
                if cancel {
                    ExecutionErrorCode::Aborted
                } else {
                    ExecutionErrorCode::Timeout
                }
            );
            assert_descendant_stopped_writing(&root).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_exec_during_cancellation_stops_descendants() {
        let (root, _dir) = temp_root();
        let env = LocalExecutionEnv::new(&root);
        let signal = CancellationToken::new();
        let options = ShellExecOptions {
            abort_signal: Some(signal.clone()),
            ..Default::default()
        };
        let command = format!("{WRITING_CHILD} & wait");
        let mut execution = env.exec(&command, Some(&options));
        let _child = tokio::select! {
            result = &mut execution => panic!("command ended before cancellation: {result:?}"),
            child = shell_child_ready(&root) => child,
        };
        signal.cancel();
        tokio::select! {
            _ = &mut execution => {},
            _ = sleep(Duration::from_millis(30)) => {},
        }
        drop(execution);
        // Drop sends SIGKILL synchronously; let an in-progress filesystem
        // write finish before checking that no further writes happen.
        sleep(Duration::from_millis(30)).await;
        assert_descendant_stopped_writing(&root).await;
    }

    #[tokio::test]
    async fn bash_tool_executes_through_the_context() {
        let (root, _guard) = temp_root();
        let tools = execution_tools(&root);
        let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
        let result = (bash.execute)(
            "call-1",
            &serde_json::json!({ "command": "echo from-bash" }),
            None,
            None,
        )
        .await
        .unwrap();
        let text = match result.content.first().unwrap() {
            holt_core::ai::types::BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(text.contains("from-bash"), "unexpected output: {text}");
    }

    #[test]
    fn bash_tool_describes_the_login_shell() {
        let (root, _guard) = temp_root();
        let tools = execution_tools(&root);
        let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
        let description = &bash.description;
        assert!(
            description.contains("login shell"),
            "description must name the login shell: {description}"
        );
        assert!(
            description.contains(login_shell().as_str()),
            "description must name the resolved shell ({}): {description}",
            login_shell()
        );
        assert!(
            description.contains("failed call"),
            "description must state that a non-zero exit is a failed call: {description}"
        );
        if login_shell().ends_with("zsh") {
            assert!(
                description.contains("quote glob patterns"),
                "zsh description must warn about unquoted globs: {description}"
            );
        }
        assert!(
            !description.contains("Execute a bash command"),
            "description still claims fixed bash execution: {description}"
        );
        assert!(
            !bash.parameters["properties"]["command"]["description"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("bash"),
            "command parameter still claims bash: {:?}",
            bash.parameters["properties"]["command"]["description"]
        );
    }
}
