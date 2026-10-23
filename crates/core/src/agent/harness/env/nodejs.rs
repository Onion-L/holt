//! Port of `pi-core/agent/src/harness/env/nodejs.ts`.
//!
//! The Node.js `fs`/`child_process` implementation of `ExecutionEnv`,
//! rebuilt on `tokio::fs` and `tokio::process`. Git Bash probing (PATH
//! lookup, then the standard install locations) is a Windows-only branch;
//! `taskkill` process-tree termination and the legacy WSL stdin transport
//! have no counterpart on this target; the Unix `/bin/bash` →
//! `which bash` → `sh` fallback chain and process-group kill are ported
//! exactly.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use super::super::types::{
    CreateDirOptions, CreateTempFileOptions, ExecutionError, ExecutionErrorCode, FileError,
    FileErrorCode, FileInfo, FileKind, FileSystem, ReadTextLinesOptions, RemoveOptions, Shell,
    ShellExecOptions, ShellExecResult, WriteContent,
};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
const MAX_TIMEOUT_SECONDS: f64 = MAX_TIMEOUT_MS / 1000.0;

/// How the command reaches the shell (`commandTransport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandTransport {
    Argv,
    Stdin,
}

struct ShellConfig {
    shell: String,
    args: Vec<String>,
    command_transport: Option<CommandTransport>,
}

fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, ExecutionError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    let timeout_ms = timeout * 1000.0;
    if timeout_ms > MAX_TIMEOUT_MS {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"),
        ));
    }
    Ok(Some(timeout_ms as u64))
}

fn home_dir() -> PathBuf {
    // HOME covers unix and git-bash launches; a stock Windows GUI process
    // carries only USERPROFILE (or HOMEDRIVE+HOMEPATH).
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(
            || match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
                (Some(drive), Some(path)) => Some(Path::new(&drive).join(&path)),
                _ => None,
            },
        )
        .unwrap_or_default()
}

/// Percent-decodes a `file://` URL body.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                output.push(byte);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Port of `fileURLToPath` for well-formed `file://` URLs; malformed URLs
/// are kept as ordinary paths so filesystem methods preserve their
/// non-throwing contract.
fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // Skip the authority component (empty or "localhost").
    let path = if rest.is_empty() || rest.starts_with('/') {
        rest
    } else {
        rest.split_once('/')?.1
    };
    if !path.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(path);
    // Node's fileURLToPath keeps the drive root: `file:///C:/x` is `C:/x`,
    // not the drive-less `/C:/x` — and the legacy pipe form spells the
    // colon `|`. Unix spellings pass through untouched.
    #[cfg(windows)]
    let decoded = {
        let bytes = decoded.as_bytes();
        if bytes.first() == Some(&b'/')
            && bytes.get(1).is_some_and(|byte| byte.is_ascii_alphabetic())
            && matches!(bytes.get(2), Some(b':') | Some(b'|'))
        {
            let mut stripped = decoded[1..].to_string();
            if stripped.as_bytes()[1] == b'|' {
                stripped.replace_range(1..2, ":");
            }
            stripped
        } else {
            decoded
        }
    };
    Some(decoded)
}

/// Lexically normalizes an absolute path the way Node's `path.resolve`
/// does (no symlink resolution).
fn normalize_absolute(path: &Path) -> String {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::Normal(part) => output.push(part),
        }
    }
    if output.as_os_str().is_empty() {
        output.push("/");
    }
    output.to_string_lossy().replace('\\', "/")
}

/// Port of `resolvePath`: `~` expansion, `file://` URLs, and lexical
/// resolution against `cwd`.
pub(crate) fn resolve_path(cwd: &str, path: &str) -> String {
    let mut normalized = path.to_string();
    if normalized == "~" {
        normalized = home_dir().to_string_lossy().into_owned();
    } else if let Some(rest) = normalized
        .strip_prefix("~/")
        .or_else(|| normalized.strip_prefix("~\\"))
    {
        normalized = home_dir().join(rest).to_string_lossy().into_owned();
    } else if let Some(decoded) = normalized
        .strip_prefix("file://")
        .and_then(|_| file_url_to_path(&normalized))
    {
        normalized = decoded;
    }

    let candidate = Path::new(&normalized);
    if candidate.is_absolute() {
        normalize_absolute(candidate)
    } else {
        normalize_absolute(&Path::new(cwd).join(candidate))
    }
}

fn file_kind_from_metadata(metadata: &std::fs::Metadata) -> Option<FileKind> {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        Some(FileKind::File)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else {
        None
    }
}

fn file_info_from_metadata(
    path: &str,
    metadata: &std::fs::Metadata,
) -> Result<FileInfo, FileError> {
    let Some(kind) = file_kind_from_metadata(metadata) else {
        return Err(FileError::with_path(
            FileErrorCode::Invalid,
            "Unsupported file type",
            path,
        ));
    };
    Ok(FileInfo {
        name: Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string()),
        path: path.to_string(),
        kind,
        size: metadata.len(),
        mtime_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or_default(),
    })
}

/// Maps `io::Error` failures onto `FileError` with the backend-independent
/// codes (`toFileError`).
fn to_file_error(error: &std::io::Error, fallback_path: Option<&str>) -> FileError {
    let path = fallback_path.map(str::to_string);
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        std::io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
        std::io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            FileErrorCode::Invalid
        }
        _ => match error.raw_os_error() {
            Some(20) => FileErrorCode::NotDirectory, // ENOTDIR
            Some(21) => FileErrorCode::IsDirectory,  // EISDIR
            Some(_) => FileErrorCode::Unknown,
            None => FileErrorCode::Unknown,
        },
    };
    FileError {
        code,
        message: error.to_string(),
        path,
    }
}

fn aborted_error(path: Option<&str>) -> FileError {
    FileError {
        code: FileErrorCode::Aborted,
        message: "aborted".to_string(),
        path: path.map(str::to_string),
    }
}

fn is_aborted(signal: Option<&CancellationToken>) -> bool {
    signal.is_some_and(|signal| signal.is_cancelled())
}

async fn path_exists(path: &str) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok()
}

fn is_legacy_wsl_bash_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_lowercase();
    normalized == "c:\\windows\\system32\\bash.exe"
        || normalized == "c:\\windows\\sysnative\\bash.exe"
}

fn get_bash_shell_config(shell: &str) -> ShellConfig {
    if is_legacy_wsl_bash_path(shell) {
        ShellConfig {
            shell: shell.to_string(),
            args: vec!["-s".to_string()],
            command_transport: Some(CommandTransport::Stdin),
        }
    } else {
        ShellConfig {
            shell: shell.to_string(),
            args: vec!["-c".to_string()],
            command_transport: None,
        }
    }
}

/// Spawn console children without allocating a console window of their own
/// (their stdio is piped; a GUI-subsystem parent would otherwise flash a
/// window per spawn).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs a short command capturing stdout (`runCommand`).
async fn run_command(command: &str, args: &[&str], timeout_ms: u64) -> (String, Option<i32>) {
    let mut builder = tokio::process::Command::new(command);
    builder
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    builder.creation_flags(CREATE_NO_WINDOW);
    let output = builder.output();
    let Ok(output) = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        Box::pin(output),
    )
    .await
    else {
        return (String::new(), None);
    };
    match output {
        Ok(output) => (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            output.status.code(),
        ),
        Err(_) => (String::new(), None),
    }
}

/// The first `where`/`which` hit for `program` that still exists on disk.
async fn first_on_path(program: &str) -> Option<String> {
    let result = if cfg!(windows) {
        run_command("where", &[program], 5000).await
    } else {
        run_command("which", &[program], 5000).await
    };
    if result.1 != Some(0) || result.0.is_empty() {
        return None;
    }
    let first_match = result
        .0
        .trim()
        .split("\r\n")
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if first_match.is_empty() || !path_exists(&first_match).await {
        return None;
    }
    Some(first_match)
}

async fn find_bash_on_path() -> Option<String> {
    first_on_path(if cfg!(windows) { "bash.exe" } else { "bash" }).await
}

/// `…\Git\cmd\git.exe` (or `…\Git\bin\git.exe`) → `…\Git\bin\bash.exe`,
/// derived without touching the filesystem; the caller checks existence.
fn bash_beside_git(git_exe: &str) -> Option<String> {
    let root = Path::new(git_exe).parent()?.parent()?;
    Some(
        root.join("bin")
            .join("bash.exe")
            .to_string_lossy()
            .into_owned(),
    )
}

/// The standard Git-for-Windows install locations of `bin\bash.exe`, most
/// canonical first, derived from the profile roots without touching the
/// filesystem.
fn standard_git_bash_locations(
    program_files: Option<&Path>,
    program_files_x86: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Vec<String> {
    program_files
        .into_iter()
        .chain(program_files_x86)
        .map(|root| root.join("Git").join("bin").join("bash.exe"))
        .chain(local_app_data.into_iter().map(|root| {
            root.join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe")
        }))
        .map(|bash| bash.to_string_lossy().into_owned())
        .collect()
}

/// Windows fallback for when no bash is on PATH: Git's stock installer
/// puts only `Git\cmd` (git.exe) on the user PATH, so a process launched
/// with the registry PATH — a GUI launch, any shell but Git Bash — sees
/// git but no bash. Derive the install root from a resolvable git.exe
/// (covers custom install drives too), then probe the standard locations.
async fn find_git_bash_off_path() -> Option<String> {
    if let Some(git_exe) = first_on_path("git.exe").await
        && let Some(beside_git) = bash_beside_git(&git_exe)
        && path_exists(&beside_git).await
    {
        return Some(beside_git);
    }
    for candidate in standard_git_bash_locations(
        std::env::var_os("ProgramFiles").as_deref().map(Path::new),
        std::env::var_os("ProgramFiles(x86)")
            .as_deref()
            .map(Path::new),
        std::env::var_os("LocalAppData").as_deref().map(Path::new),
    ) {
        if path_exists(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

async fn get_shell_config(custom_shell_path: Option<&str>) -> Result<ShellConfig, ExecutionError> {
    if let Some(custom) = custom_shell_path {
        if path_exists(custom).await {
            return Ok(get_bash_shell_config(custom));
        }
        return Err(ExecutionError::new(
            ExecutionErrorCode::ShellUnavailable,
            format!("Custom shell path not found: {custom}"),
        ));
    }
    if cfg!(windows) {
        // PATH keeps priority: a WSL `System32\bash.exe` found there is
        // deliberately supported (stdin transport above), so the Git Bash
        // probe must not leapfrog it.
        if let Some(bash_on_path) = find_bash_on_path().await {
            return Ok(get_bash_shell_config(&bash_on_path));
        }
        if let Some(git_bash) = find_git_bash_off_path().await {
            return Ok(get_bash_shell_config(&git_bash));
        }
        return Err(ExecutionError::new(
            ExecutionErrorCode::ShellUnavailable,
            "No bash shell found.",
        ));
    }

    if path_exists("/bin/bash").await {
        return Ok(get_bash_shell_config("/bin/bash"));
    }
    if let Some(bash_on_path) = find_bash_on_path().await {
        return Ok(get_bash_shell_config(&bash_on_path));
    }
    Ok(ShellConfig {
        shell: "sh".to_string(),
        args: vec!["-c".to_string()],
        command_transport: None,
    })
}

fn get_shell_env(
    base_env: Option<&BTreeMap<String, String>>,
    extra_env: Option<&BTreeMap<String, String>>,
    inherit_env: bool,
) -> BTreeMap<String, String> {
    // Not inheriting also drops the configured base shell environment,
    // exactly like the TypeScript spread (`{ ...extraEnv }`).
    let mut merged = BTreeMap::new();
    if inherit_env {
        for (key, value) in std::env::vars() {
            merged.insert(key, value);
        }
        if let Some(base_env) = base_env {
            merged.extend(base_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
    if let Some(extra_env) = extra_env {
        merged.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    merged
}

/// Port of `killProcessTree`: kill the child's whole process group, then
/// the process itself.
fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        let pid = pid as i32;
        if libc::kill(-pid, libc::SIGKILL) == -1 && libc::kill(pid, libc::SIGKILL) == -1 {
            // Process already dead.
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

/// Incrementally decodes a UTF-8 byte stream into string chunks, carrying
/// partial multi-byte sequences across reads.
#[derive(Default)]
struct IncrementalUtf8Decoder {
    buffer: Vec<u8>,
}

impl IncrementalUtf8Decoder {
    fn push(&mut self, chunk: &[u8]) -> String {
        self.buffer.extend_from_slice(chunk);
        let decoded = match std::str::from_utf8(&self.buffer) {
            Ok(_) => {
                let text = String::from_utf8_lossy(&self.buffer).into_owned();
                self.buffer.clear();
                return text;
            }
            Err(error) => error.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&self.buffer[..decoded]).into_owned();
        self.buffer.drain(..decoded);
        text
    }
}

fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system RNG is always available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Port of `NodeExecutionEnv`.
pub struct NodeExecutionEnv {
    cwd: String,
    shell_path: Option<String>,
    shell_env: Option<BTreeMap<String, String>>,
    active_child_pids: Arc<Mutex<HashSet<u32>>>,
}

impl NodeExecutionEnv {
    pub fn new(options: NodeExecutionEnvOptions) -> Self {
        Self {
            cwd: options.cwd,
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            active_child_pids: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

/// Constructor options for [`NodeExecutionEnv`].
#[derive(Default)]
pub struct NodeExecutionEnvOptions {
    pub cwd: String,
    pub shell_path: Option<String>,
    pub shell_env: Option<BTreeMap<String, String>>,
}

impl FileSystem for NodeExecutionEnv {
    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move { Ok(resolve_path(&self.cwd, path)) })
    }

    fn join_path<'a>(
        &'a self,
        parts: &'a [String],
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let mut joined = PathBuf::new();
            for part in parts {
                joined.push(part);
            }
            // The addressed vocabulary (`/` separators) — a native join on
            // Windows would mix separators whenever the left side already
            // spells addressed.
            Ok(normalize_absolute(&joined))
        })
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            match tokio::fs::read(&resolved).await {
                Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a str,
        options: ReadTextLinesOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<String>, FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            if let Some(max_lines) = options.max_lines
                && max_lines == 0
            {
                return Ok(Vec::new());
            }
            let file = match tokio::fs::File::open(&resolved).await {
                Ok(file) => file,
                Err(error) => return Err(to_file_error(&error, Some(&resolved))),
            };
            let mut lines: Vec<String> = Vec::new();
            let mut carry = String::new();
            let mut decoder = IncrementalUtf8Decoder::default();
            let mut reader = tokio::io::BufReader::new(file);
            let mut bytes = Vec::new();
            use tokio::io::AsyncReadExt;
            loop {
                bytes.clear();
                match reader.read_buf(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if is_aborted(abort_signal.as_ref()) {
                            return Err(aborted_error(Some(&resolved)));
                        }
                        carry.push_str(&decoder.push(&bytes));
                        while let Some(newline) = carry.find('\n') {
                            let line: String = carry.drain(..=newline).collect();
                            lines.push(
                                line.trim_end_matches('\n')
                                    .trim_end_matches('\r')
                                    .to_string(),
                            );
                            if let Some(max_lines) = options.max_lines
                                && lines.len() >= max_lines
                            {
                                return Ok(lines);
                            }
                        }
                    }
                }
            }
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            // The final line may lack a trailing newline; readline still
            // yields it.
            if !carry.is_empty() {
                lines.push(carry.trim_end_matches('\r').to_string());
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
            let resolved = resolve_path(&self.cwd, path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            match tokio::fs::read(&resolved).await {
                Ok(bytes) => Ok(bytes),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            let parent = Path::new(&resolved)
                .parent()
                .map(|parent| parent.to_path_buf())
                .unwrap_or_default();
            if let Some(error) = tokio::fs::create_dir_all(&parent).await.err() {
                return Err(to_file_error(&error, Some(&resolved)));
            }
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            let write = match content {
                WriteContent::Text(text) => tokio::fs::write(&resolved, text.as_bytes()).await,
                WriteContent::Bytes(bytes) => tokio::fs::write(&resolved, bytes).await,
            };
            match write {
                Ok(()) => Ok(()),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            let parent = Path::new(&resolved)
                .parent()
                .map(|parent| parent.to_path_buf())
                .unwrap_or_default();
            if let Some(error) = tokio::fs::create_dir_all(&parent).await.err() {
                return Err(to_file_error(&error, Some(&resolved)));
            }
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&resolved)
                .await
            {
                Ok(file) => file,
                Err(error) => return Err(to_file_error(&error, Some(&resolved))),
            };
            let write = match content {
                WriteContent::Text(text) => file.write_all(text.as_bytes()).await,
                WriteContent::Bytes(bytes) => file.write_all(bytes).await,
            };
            // `tokio::fs::File::write_all` resolves once the bytes are in
            // tokio's buffer; the blocking write(2) runs afterwards, so a
            // follow-up append on a fresh handle could land first. Node's
            // `fs.appendFile` resolves only once the write reached the OS;
            // `flush` restores that by waiting out the background write.
            let write = match write {
                Ok(()) => file.flush().await,
                Err(error) => Err(error),
            };
            match write {
                Ok(()) => Ok(()),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn rename_file<'a>(
        &'a self,
        source_path: &'a str,
        destination_path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let source = resolve_path(&self.cwd, source_path);
            let destination = resolve_path(&self.cwd, destination_path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&destination)));
            }
            match tokio::fs::rename(&source, &destination).await {
                Ok(()) => Ok(()),
                Err(error) => Err(to_file_error(&error, Some(&source))),
            }
        })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<FileInfo, FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            match tokio::fs::symlink_metadata(&resolved).await {
                Ok(metadata) => file_info_from_metadata(&resolved, &metadata),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            if is_aborted(abort_signal.as_ref()) {
                return Err(aborted_error(Some(&resolved)));
            }
            let mut entries = match tokio::fs::read_dir(&resolved).await {
                Ok(entries) => entries,
                Err(error) => return Err(to_file_error(&error, Some(&resolved))),
            };
            let mut infos: Vec<FileInfo> = Vec::new();
            let mut entry_paths: Vec<String> = Vec::new();
            loop {
                let entry = match entries.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(error) => return Err(to_file_error(&error, Some(&resolved))),
                };
                if is_aborted(abort_signal.as_ref()) {
                    return Err(aborted_error(Some(&resolved)));
                }
                // Addressed vocabulary: the OS join would append a native
                // separator to an addressed-spelled directory.
                entry_paths.push(normalize_absolute(&entry.path()));
            }
            for entry_path in entry_paths {
                match tokio::fs::symlink_metadata(&entry_path).await {
                    Ok(metadata) => {
                        if let Ok(info) = file_info_from_metadata(&entry_path, &metadata) {
                            infos.push(info);
                        }
                    }
                    Err(error) => return Err(to_file_error(&error, Some(&entry_path))),
                }
            }
            Ok(infos)
        })
    }

    fn canonical_path<'a>(
        &'a self,
        path: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            match tokio::fs::canonicalize(&resolved).await {
                Ok(canonical) => Ok(canonical.to_string_lossy().into_owned()),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a str,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<bool, FileError>> {
        Box::pin(async move {
            match self.file_info(path, abort_signal).await {
                Ok(_) => Ok(true),
                Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a str,
        options: CreateDirOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            let recursive = options.recursive.unwrap_or(true);
            let result = if recursive {
                tokio::fs::create_dir_all(&resolved).await
            } else {
                tokio::fs::create_dir(&resolved).await
            };
            match result {
                Ok(()) => Ok(()),
                Err(error) => Err(to_file_error(&error, Some(&resolved))),
            }
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a str,
        options: RemoveOptions,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<(), FileError>> {
        Box::pin(async move {
            let resolved = resolve_path(&self.cwd, path);
            let recursive = options.recursive.unwrap_or(false);
            let force = options.force.unwrap_or(false);
            let result = if recursive {
                tokio::fs::remove_dir_all(&resolved).await
            } else {
                tokio::fs::remove_file(&resolved).await
            };
            match result {
                Ok(()) => Ok(()),
                Err(error) => {
                    if force && matches!(to_file_error(&error, None).code, FileErrorCode::NotFound)
                    {
                        return Ok(());
                    }
                    Err(to_file_error(&error, Some(&resolved)))
                }
            }
        })
    }

    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
        _abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let base = std::env::temp_dir().join(prefix);
            loop {
                let candidate = base.with_file_name(format!("{prefix}{}", random_hex()));
                match tokio::fs::create_dir(&candidate).await {
                    Ok(()) => return Ok(candidate.to_string_lossy().into_owned()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(to_file_error(&error, None)),
                }
            }
        })
    }

    fn create_temp_file<'a>(
        &'a self,
        options: &'a CreateTempFileOptions,
        abort_signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            let dir = match self.create_temp_dir("tmp-", abort_signal.clone()).await {
                Ok(dir) => dir,
                Err(error) => return Err(error),
            };
            let file_path = Path::new(&dir).join(format!(
                "{}{}{}",
                options.prefix.as_deref().unwrap_or(""),
                random_hex(),
                options.suffix.as_deref().unwrap_or("")
            ));
            match tokio::fs::write(&file_path, b"").await {
                Ok(()) => Ok(file_path.to_string_lossy().into_owned()),
                Err(error) => Err(to_file_error(&error, Some(&file_path.to_string_lossy()))),
            }
        })
    }

    fn cleanup(&self) -> BoxFuture<'static, ()> {
        let pids = Arc::clone(&self.active_child_pids);
        Box::pin(async move {
            let active: Vec<u32> = pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain()
                .collect();
            for pid in active {
                kill_process_tree(pid);
            }
        })
    }
}

impl Shell for NodeExecutionEnv {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        options: Option<&'a ShellExecOptions>,
    ) -> BoxFuture<'a, Result<ShellExecResult, ExecutionError>> {
        Box::pin(async move {
            let options = options.cloned().unwrap_or_default();
            if is_aborted(options.abort_signal.as_ref()) {
                return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
            }
            let timeout_ms = resolve_timeout_ms(options.timeout)?;
            let cwd = match &options.cwd {
                Some(cwd) => resolve_path(&self.cwd, cwd),
                None => self.cwd.clone(),
            };
            let shell_config = get_shell_config(self.shell_path.as_deref()).await?;
            if !path_exists(&cwd).await {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::SpawnError,
                    format!(
                        "Working directory does not exist: {cwd}\nCannot execute bash commands."
                    ),
                ));
            }

            let command_from_stdin =
                shell_config.command_transport == Some(CommandTransport::Stdin);
            let mut child_builder = tokio::process::Command::new(&shell_config.shell);
            // The TypeScript spawn replaces the whole child environment.
            child_builder
                .current_dir(&cwd)
                .env_clear()
                .envs(get_shell_env(
                    self.shell_env.as_ref(),
                    options.env.as_ref(),
                    options.inherit_env.unwrap_or(true),
                ))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(unix)]
            child_builder.process_group(0);
            #[cfg(windows)]
            child_builder.creation_flags(CREATE_NO_WINDOW);
            for arg in &shell_config.args {
                child_builder.arg(arg);
            }
            if command_from_stdin {
                child_builder.stdin(std::process::Stdio::piped());
            } else {
                child_builder.arg(command);
            }

            let mut child = match child_builder.spawn() {
                Ok(child) => child,
                Err(error) => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        error.to_string(),
                    ));
                }
            };
            let pid = child.id();
            if let Some(pid) = pid {
                self.active_child_pids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(pid);
            }
            let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(pid) = pid {
                    kill_process_tree(pid);
                }
            });

            if command_from_stdin && let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(command.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let callback_error: Arc<Mutex<Option<ExecutionError>>> = Arc::new(Mutex::new(None));
            let stdout_chunk_listener = options.on_stdout.clone();
            let stderr_chunk_listener = options.on_stderr.clone();

            let stdout_kill = Arc::clone(&kill);
            let stderr_kill = Arc::clone(&kill);
            let stdout_callback_error = Arc::clone(&callback_error);
            let stderr_callback_error = Arc::clone(&callback_error);
            let stdout_task = stdout.map(|stdout| async move {
                let mut output = String::new();
                let mut decoder = IncrementalUtf8Decoder::default();
                read_stream_chunks_for_shell(
                    tokio::io::BufReader::new(stdout),
                    stdout_chunk_listener.as_ref(),
                    &mut output,
                    &mut decoder,
                    &stdout_callback_error,
                    &stdout_kill,
                )
                .await;
                output
            });
            let stderr_task = stderr.map(|stderr| async move {
                let mut output = String::new();
                let mut decoder = IncrementalUtf8Decoder::default();
                read_stream_chunks_for_shell(
                    tokio::io::BufReader::new(stderr),
                    stderr_chunk_listener.as_ref(),
                    &mut output,
                    &mut decoder,
                    &stderr_callback_error,
                    &stderr_kill,
                )
                .await;
                output
            });

            let mut timed_out = false;
            let mut completion = Box::pin(async {
                let stdout = match stdout_task {
                    Some(task) => task.await,
                    None => String::new(),
                };
                let stderr = match stderr_task {
                    Some(task) => task.await,
                    None => String::new(),
                };
                let status = child.wait().await;
                (stdout, stderr, status)
            });

            let mut completed = false;
            let mut result: (String, String, std::io::Result<std::process::ExitStatus>) = (
                String::new(),
                String::new(),
                Err(std::io::Error::other("unset")),
            );
            {
                let abort_watch = async {
                    match options.abort_signal.as_ref() {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending().await,
                    }
                };
                let timeout_watch = async {
                    match timeout_ms {
                        Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                        None => std::future::pending().await,
                    }
                };
                let needs_watch = options.abort_signal.is_some() || timeout_ms.is_some();
                if needs_watch {
                    tokio::select! {
                        _ = abort_watch => { kill(); }
                        _ = timeout_watch => { timed_out = true; kill(); }
                        finished = &mut completion => { completed = true; result = finished; }
                    }
                }
            }
            if !completed {
                result = completion.as_mut().await;
            }

            if let Some(pid) = pid {
                self.active_child_pids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&pid);
            }

            if let Some(callback_error) = callback_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                return Err(callback_error);
            }
            if timed_out {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!("timeout:{}", options.timeout.unwrap_or_default()),
                ));
            }
            if is_aborted(options.abort_signal.as_ref()) {
                return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
            }
            let status = match result.2 {
                Ok(status) => status,
                Err(error) => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        error.to_string(),
                    ));
                }
            };
            Ok(ShellExecResult {
                stdout: result.0,
                stderr: result.1,
                exit_code: status.code().unwrap_or(0),
            })
        })
    }

    fn cleanup(&self) -> BoxFuture<'static, ()> {
        FileSystem::cleanup(self)
    }
}

/// Shared chunk reader used by both stdio streams (adapter over
/// [`read_stream_chunks`] with the concrete child stream types).
async fn read_stream_chunks_for_shell<R: tokio::io::AsyncRead + Unpin>(
    mut reader: tokio::io::BufReader<R>,
    on_chunk: Option<&super::super::types::ChunkListener>,
    output: &mut String,
    decoder: &mut IncrementalUtf8Decoder,
    callback_error: &Arc<Mutex<Option<ExecutionError>>>,
    kill: &Arc<dyn Fn() + Send + Sync>,
) {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_buf(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let chunk = decoder.push(&bytes);
                output.push_str(&chunk);
                if let Some(on_chunk) = on_chunk
                    && let Err(message) = on_chunk(&chunk)
                {
                    *callback_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                        ExecutionError::new(ExecutionErrorCode::CallbackError, message),
                    );
                    kill();
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_beside_git_targets_the_install_roots_bin() {
        // `cmd\git.exe` and `bin\git.exe` both sit two levels under the
        // install root; a bare name has no root to derive from. Compared
        // as `Path`s so the assertion holds under either separator.
        assert_eq!(
            bash_beside_git("C:/Program Files/Git/cmd/git.exe")
                .as_deref()
                .map(Path::new),
            Some(Path::new("C:/Program Files/Git/bin/bash.exe"))
        );
        assert_eq!(
            bash_beside_git("D:/tools/git/bin/git.exe")
                .as_deref()
                .map(Path::new),
            Some(Path::new("D:/tools/git/bin/bash.exe"))
        );
        assert_eq!(bash_beside_git("git.exe").as_deref().map(Path::new), None);
    }

    #[test]
    fn standard_locations_cover_both_program_files_and_per_user_installs() {
        let locations = standard_git_bash_locations(
            Some(Path::new("C:/Program Files")),
            Some(Path::new("C:/Program Files (x86)")),
            Some(Path::new("C:/Users/u/AppData/Local")),
        );
        let as_paths: Vec<&Path> = locations.iter().map(|p| Path::new(p.as_str())).collect();
        assert_eq!(
            as_paths,
            vec![
                Path::new("C:/Program Files/Git/bin/bash.exe"),
                Path::new("C:/Program Files (x86)/Git/bin/bash.exe"),
                Path::new("C:/Users/u/AppData/Local/Programs/Git/bin/bash.exe"),
            ]
        );
    }

    #[test]
    fn standard_locations_skip_missing_roots() {
        let locations =
            standard_git_bash_locations(Some(Path::new("C:/Program Files")), None, None);
        assert_eq!(locations.len(), 1);
    }
}
