//! The agent's execution tools: the pi-core built-ins (read/write/edit/
//! bash) mounted onto the local machine — a [`LocalExecutionEnv`] satisfying
//! pi-core's `FileSystem + Shell` contract, and the assembly that hands each
//! harness tool the shared [`ExecutionToolContext`] its `execute` downcasts
//! for — plus holt's own content search (ADR-0004) in [`grep`].

mod grep;

use std::{future::pending, path::Path, process::Stdio, sync::Arc};

use futures::future::BoxFuture;
use pi_core::agent::{
    harness::{
        tools::{
            bash::{BashToolOptions, create_bash_tool},
            edit::create_edit_tool,
            read::{ReadToolOptions, create_read_tool},
            tool_context::ExecutionToolContext,
            write::create_write_tool,
        },
        types::{
            AgentHarnessTool, AgentToolContext, CreateDirOptions, CreateTempFileOptions,
            ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo,
            FileKind, FileSystem, ReadTextLinesOptions, RemoveOptions, Shell, ShellExecOptions,
            ShellExecResult, WriteContent,
        },
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

/// The host filesystem and shell, rooted at the chat's working directory.
/// Blocking std filesystem calls run inline — the pi-core tools only issue
/// small reads/writes, and `exec` (the long-running case) is fully async.
/// Content search, the one long-running local operation that is not an env
/// method, runs its walk on `spawn_blocking` instead (see [`grep`]).
pub(crate) struct LocalExecutionEnv {
    cwd: String,
}

impl LocalExecutionEnv {
    pub(crate) fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
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

fn write_to_file(path: &str, content: &WriteContent, append: bool) -> Result<(), FileError> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_file_error(&error, &format!("{}/", parent.display())))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|error| io_file_error(&error, path))?;
    match content {
        WriteContent::Text(text) => std::io::Write::write_all(&mut file, text.as_bytes()),
        WriteContent::Bytes(bytes) => std::io::Write::write_all(&mut file, bytes),
    }
    .map_err(|error| io_file_error(&error, path))
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
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileError>> {
        Box::pin(async move {
            let path = to_absolute(&self.cwd, path);
            std::fs::read(&path).map_err(|error| io_file_error(&error, &path))
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
            write_to_file(&path, content, false)
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
            write_to_file(&path, content, true)
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
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(command)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // The child handle is owned by the select block below and
                // dropped on the abort/timeout branches — dropping kills it.
                .kill_on_drop(true);
            match options.and_then(|options| options.inherit_env) {
                Some(false) => {
                    cmd.env_clear();
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
            // Readers own their pipes; capped so `cat huge.log` cannot balloon
            // memory ahead of the bash tool's own truncation.
            async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut pipe: Option<R>) -> String {
                let mut buffer = Vec::new();
                if let Some(pipe) = pipe.as_mut() {
                    let _ = pipe.take(EXEC_OUTPUT_CAP).read_to_end(&mut buffer).await;
                }
                String::from_utf8_lossy(&buffer).into_owned()
            }
            let stdout_task = tokio::spawn(read_capped(child.stdout.take()));
            let stderr_task = tokio::spawn(read_capped(child.stderr.take()));

            let aborted = options.and_then(|options| options.abort_signal.clone());
            let timeout = options
                .and_then(|options| options.timeout)
                .map(Duration::from_secs_f64);
            let status = tokio::select! {
                _ = async {
                    match aborted.as_ref() {
                        Some(signal) => signal.cancelled().await,
                        None => pending::<()>().await,
                    }
                } => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Aborted,
                        "command aborted",
                    ));
                }
                _ = async {
                    match timeout {
                        Some(budget) => sleep(budget).await,
                        None => pending::<()>().await,
                    }
                } => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Timeout,
                        format!("command exceeded its timeout: {command}"),
                    ));
                }
                status = child.wait() => status.map_err(|error| io_execution_error(&error))?,
            };
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
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

/// The toolset handed to the agent loop: pi-core's built-in read/write/
/// edit/bash, all running against one environment rooted at `cwd`, plus
/// holt's own content-search tool, exposed to the agent as `grep`.
pub(crate) fn execution_tools(cwd: &str) -> Vec<AgentTool> {
    let env: Arc<dyn ExecutionEnv> = Arc::new(LocalExecutionEnv::new(cwd));
    let context = ExecutionToolContext { env }.into_tool_context();
    vec![
        with_execution_context(create_read_tool(ReadToolOptions::default()), &context),
        with_execution_context(create_write_tool(), &context),
        with_execution_context(create_edit_tool(), &context),
        with_execution_context(create_bash_tool(BashToolOptions::default()), &context),
        grep::create_grep_tool(cwd),
    ]
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
            pi_core::ai::types::BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(text.contains("from-bash"), "unexpected output: {text}");
    }
}
