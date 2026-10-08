//! Port of `pi-core/agent/src/harness/types.ts`.
//!
//! TypeScript models fallible harness operations with a `Result` union and
//! tagged error classes; the Rust port uses `std::result::Result` with
//! concrete error structs (`FileError`, `ExecutionError`, …) whose `code`
//! fields carry the backend-independent codes. The `TaggedError` factory
//! from `result.ts` has no Rust counterpart — the concrete error types and
//! `match` on their codes serve the same purpose.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::ai::types::{CacheRetention, Transport};

pub use crate::agent::types::AgentToolResult;

/// Port of `Skill`: a skill loaded from a `SKILL.md` file or provided by an
/// application.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Skill {
    /// Stable skill name used for lookup and model-visible listings.
    pub name: String,
    /// Short model-visible description of when to use the skill.
    pub description: String,
    /// Full skill instructions.
    pub content: String,
    /// Absolute path to the skill file.
    pub file_path: String,
    /// Exclude this skill from model-visible skill lists while still
    /// allowing explicit application invocation.
    pub disable_model_invocation: Option<bool>,
}

/// Port of `PromptTemplate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptTemplate {
    /// Stable template name used for lookup or command routing.
    pub name: String,
    /// Optional description for command lists or autocomplete.
    pub description: Option<String>,
    /// Template content; argument placeholders are formatted by
    /// `format_prompt_template_invocation`.
    pub content: String,
}

/// Port of `AgentHarnessResources`.
#[derive(Clone, Debug, Default)]
pub struct AgentHarnessResources {
    pub prompt_templates: Option<Vec<PromptTemplate>>,
    pub skills: Option<Vec<Skill>>,
}

/// Context passed to harness tool execution (the `context` parameter added
/// by `AgentHarnessTool.execute`).
pub type AgentToolContext = Arc<dyn std::any::Any + Send + Sync>;

/// The `execute` member of `AgentHarnessTool`.
pub type AgentHarnessToolExecuteFn = Arc<
    dyn Fn(
            &str,
            &serde_json::Value,
            Option<&CancellationToken>,
            Option<&crate::agent::types::AgentToolUpdateCallback>,
            &AgentToolContext,
        ) -> BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// Port of `AgentHarnessTool`: an [`crate::agent::types::AgentTool`] whose
/// execute receives the context resolved for the current turn snapshot.
///
/// The Rust port models this as the plain `AgentTool` plus an
/// application-owned context value threaded through the harness; tools
/// downcast `AgentToolContext` to their expected type.
#[derive(Clone)]
pub struct AgentHarnessTool {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub constrained_sampling: Option<crate::ai::types::ToolConstrainedSampling>,
    pub prepare_arguments: Option<crate::agent::types::PrepareArgumentsFn>,
    pub execution_mode: Option<crate::agent::types::ToolExecutionMode>,
    /// Whether completed calls may be replayed while rebuilding a run.
    pub replay: Option<crate::agent::harness::agent_harness::ToolReplay>,
    /// Executes the tool call with the context resolved for the current
    /// turn snapshot.
    pub execute: AgentHarnessToolExecuteFn,
}

impl AgentHarnessTool {
    /// The equivalent plain agent tool, dropping the context parameter.
    pub fn to_agent_tool(&self) -> crate::agent::types::AgentTool {
        crate::agent::types::AgentTool {
            name: self.name.clone(),
            label: self.label.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            constrained_sampling: self.constrained_sampling.clone(),
            prepare_arguments: self.prepare_arguments.clone(),
            execution_mode: self.execution_mode,
            execute: Arc::new({
                let execute = Arc::clone(&self.execute);
                move |tool_call_id, params, signal, on_update| {
                    let context: AgentToolContext = Arc::new(());
                    (execute)(tool_call_id, params, signal, on_update, &context)
                }
            }),
        }
    }
}

/// Port of `AgentHarnessStreamOptions`.
#[derive(Clone, Debug, Default)]
pub struct AgentHarnessStreamOptions {
    pub transport: Option<Transport>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    pub cache_retention: Option<CacheRetention>,
}

/// Port of `AgentHarnessStreamOptionsPatch`.
#[derive(Clone, Debug, Default)]
pub struct AgentHarnessStreamOptionsPatch {
    pub transport: Option<Transport>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    /// Header patch; `None` values delete keys.
    pub headers: Option<std::collections::BTreeMap<String, Option<String>>>,
    /// Metadata patch; `None` values delete keys.
    pub metadata: Option<std::collections::BTreeMap<String, Option<serde_json::Value>>>,
    pub cache_retention: Option<CacheRetention>,
}

/// Port of `FileKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Port of `FileErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

/// Port of `FileError`.
#[derive(Clone, Debug, PartialEq)]
pub struct FileError {
    /// Backend-independent error code.
    pub code: FileErrorCode,
    pub message: String,
    /// Absolute addressed path associated with the failure, when available.
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }

    pub fn with_path(
        code: FileErrorCode,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            path: Some(path.into()),
        }
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(
                f,
                "{}: {} ({})",
                error_code_name(self.code),
                self.message,
                path
            ),
            None => write!(f, "{}: {}", error_code_name(self.code), self.message),
        }
    }
}

impl std::error::Error for FileError {}

fn error_code_name(code: FileErrorCode) -> &'static str {
    match code {
        FileErrorCode::Aborted => "aborted",
        FileErrorCode::NotFound => "not_found",
        FileErrorCode::PermissionDenied => "permission_denied",
        FileErrorCode::NotDirectory => "not_directory",
        FileErrorCode::IsDirectory => "is_directory",
        FileErrorCode::Invalid => "invalid",
        FileErrorCode::NotSupported => "not_supported",
        FileErrorCode::Unknown => "unknown",
    }
}

/// Port of `ExecutionErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

/// Port of `ExecutionError`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionError {
    /// Backend-independent error code.
    pub code: ExecutionErrorCode,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.code {
            ExecutionErrorCode::Aborted => "aborted",
            ExecutionErrorCode::Timeout => "timeout",
            ExecutionErrorCode::ShellUnavailable => "shell_unavailable",
            ExecutionErrorCode::SpawnError => "spawn_error",
            ExecutionErrorCode::CallbackError => "callback_error",
            ExecutionErrorCode::Unknown => "unknown",
        };
        write!(f, "{name}: {}", self.message)
    }
}

impl std::error::Error for ExecutionError {}

/// Port of `CompactionErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionErrorCode {
    Aborted,
    SummarizationFailed,
}

/// Port of `CompactionError`.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionError {
    pub code: CompactionErrorCode,
    pub message: String,
}

impl CompactionError {
    pub fn new(code: CompactionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.code {
            CompactionErrorCode::Aborted => "aborted",
            CompactionErrorCode::SummarizationFailed => "summarization_failed",
        };
        write!(f, "{name}: {}", self.message)
    }
}

impl std::error::Error for CompactionError {}

/// Port of `BranchSummaryErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchSummaryErrorCode {
    Aborted,
    SummarizationFailed,
}

/// Port of `BranchSummaryError`.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummaryError {
    pub code: BranchSummaryErrorCode,
    pub message: String,
}

impl BranchSummaryError {
    pub fn new(code: BranchSummaryErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BranchSummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.code {
            BranchSummaryErrorCode::Aborted => "aborted",
            BranchSummaryErrorCode::SummarizationFailed => "summarization_failed",
        };
        write!(f, "{name}: {}", self.message)
    }
}

impl std::error::Error for BranchSummaryError {}

/// Port of `FileInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    /// Basename of `path`.
    pub name: String,
    /// Absolute, syntactically normalized addressed path.
    pub path: String,
    /// Object kind; symlink targets are not followed.
    pub kind: FileKind,
    /// Size in bytes for the addressed filesystem object.
    pub size: u64,
    /// Modification time as milliseconds since Unix epoch.
    pub mtime_ms: f64,
}

/// File content accepted by write/append operations (`string | Uint8Array`).
#[derive(Clone, Debug, PartialEq)]
pub enum WriteContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl From<&str> for WriteContent {
    fn from(text: &str) -> Self {
        WriteContent::Text(text.to_string())
    }
}

impl From<String> for WriteContent {
    fn from(text: String) -> Self {
        WriteContent::Text(text)
    }
}

impl From<Vec<u8>> for WriteContent {
    fn from(bytes: Vec<u8>) -> Self {
        WriteContent::Bytes(bytes)
    }
}

/// Options for `FileSystem::read_text_lines`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadTextLinesOptions {
    pub max_lines: Option<usize>,
}

/// Options for `FileSystem::create_dir`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateDirOptions {
    pub recursive: Option<bool>,
}

/// Options for `FileSystem::remove`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoveOptions {
    pub recursive: Option<bool>,
    pub force: Option<bool>,
}

/// Options for `FileSystem::create_temp_file`.
#[derive(Clone, Debug, Default)]
pub struct CreateTempFileOptions {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

/// A streaming output chunk listener.
///
/// TypeScript listeners are typed `void` but may throw at runtime; the
/// Rust port surfaces that failure mode as the returned error (mapped to
/// `callback_error` by the execution env).
pub type ChunkListener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Port of `ShellExecOptions`.
#[derive(Clone, Default)]
pub struct ShellExecOptions {
    /// Working directory for the command.
    pub cwd: Option<String>,
    /// Environment variables for the command.
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Whether to inherit the execution environment's defaults (default
    /// true).
    pub inherit_env: Option<bool>,
    /// Timeout in seconds.
    pub timeout: Option<f64>,
    /// Abort signal used to terminate the command.
    pub abort_signal: Option<CancellationToken>,
    /// Called with stdout chunks as they are produced.
    pub on_stdout: Option<ChunkListener>,
    /// Called with stderr chunks as they are produced.
    pub on_stderr: Option<ChunkListener>,
}

/// The result of `Shell::exec`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShellExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Port of the `FileSystem` capability.
///
/// Operation methods never panic; all filesystem failures are encoded in
/// the returned `Result`.
pub trait FileSystem: Send + Sync {
    /// Current working directory for relative paths.
    fn cwd(&self) -> String;

    /// Return an absolute addressed path without requiring it to exist and
    /// without resolving symlinks.
    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Join path segments in the filesystem namespace.
    fn join_path<'a>(
        &'a self,
        parts: &'a [String],
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Read a UTF-8 text file.
    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Read UTF-8 text lines, stopping after `max_lines` lines.
    fn read_text_lines<'a>(
        &'a self,
        path: &'a str,
        options: ReadTextLinesOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<String>, FileError>>;

    /// Read a binary file.
    fn read_binary_file<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileError>>;

    /// Create or overwrite a file, creating parent directories when
    /// supported.
    fn write_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>>;

    /// Create or append to a file, creating parent directories when
    /// supported.
    fn append_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>>;

    /// Atomically rename a file, replacing the destination when it exists.
    fn rename_file<'a>(
        &'a self,
        source_path: &'a str,
        destination_path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>>;

    /// Return metadata for the addressed path without following symlinks.
    fn file_info<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<FileInfo, FileError>>;

    /// List direct children of a directory without following symlinks.
    fn list_dir<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, FileError>>;

    /// Return the canonical path for an existing path, resolving symlinks
    /// where supported.
    fn canonical_path<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Return false for missing paths; other errors return a `FileError`.
    fn exists<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<bool, FileError>>;

    /// Create a directory. Defaults: recursive.
    fn create_dir<'a>(
        &'a self,
        path: &'a str,
        options: CreateDirOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>>;

    /// Remove a file or directory. Defaults: not recursive, not forced.
    fn remove<'a>(
        &'a self,
        path: &'a str,
        options: RemoveOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>>;

    /// Create a temporary directory and return its absolute path.
    /// Defaults: prefix `"tmp-"`.
    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Create a temporary file and return its absolute path.
    fn create_temp_file<'a>(
        &'a self,
        options: &'a CreateTempFileOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>>;

    /// Release filesystem resources; best-effort.
    fn cleanup(&self) -> BoxFuture<'static, ()>;
}

/// Port of the `Shell` capability.
pub trait Shell: Send + Sync {
    /// Execute a shell command in `FileSystem::cwd` unless `options.cwd`
    /// is provided.
    fn exec<'a>(
        &'a self,
        command: &'a str,
        options: Option<&'a ShellExecOptions>,
    ) -> BoxFuture<'a, Result<ShellExecResult, ExecutionError>>;

    /// Release shell resources; best-effort.
    fn cleanup(&self) -> BoxFuture<'static, ()>;
}

/// Port of `ExecutionEnv`: filesystem and process execution environment.
pub trait ExecutionEnv: FileSystem + Shell {}

impl<T: FileSystem + Shell> ExecutionEnv for T {}
