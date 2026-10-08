//! Port of `pi-core/agent/src/harness/utils/shell-output.ts`.
//!
//! Deviations: the TypeScript `onChunk(chunk, getProgress)` callback
//! receives a progress getter; the Rust callback receives the progress
//! snapshot computed for that chunk (identical values, since progress
//! state does not change during a single callback). The TypeScript
//! try/catch around chunk processing (captureError) guards callback
//! throws that the infallible Rust callback type cannot produce.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use super::super::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, ShellExecOptions, WriteContent,
};
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationResult, truncate_tail,
};

/// Port of `ShellCaptureProgress`.
#[derive(Clone, Debug)]
pub struct ShellCaptureProgress {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<String>,
    pub last_line_bytes: usize,
}

/// The `onChunk` callback: receives the sanitized chunk text and the
/// progress snapshot for that chunk.
pub type ShellCaptureChunkCallback = Arc<dyn Fn(&str, ShellCaptureProgress) + Send + Sync>;

/// Port of `ShellCaptureOptions`.
#[derive(Clone, Default)]
pub struct ShellCaptureOptions {
    pub cwd: Option<String>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub inherit_env: Option<bool>,
    pub timeout: Option<f64>,
    pub abort_signal: Option<CancellationToken>,
    pub on_chunk: Option<ShellCaptureChunkCallback>,
    /// Return shell execution failures with captured output instead of as
    /// a failed result.
    pub return_execution_errors: Option<bool>,
}

/// Port of `ShellCaptureResult`.
#[derive(Clone, Debug)]
pub struct ShellCaptureResult {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<String>,
    pub last_line_bytes: usize,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub execution_error: Option<ExecutionError>,
}

/// Port of `sanitizeBinaryOutput`: strips control characters except tab,
/// newline, and carriage return, plus the interlinear annotation
/// characters.
pub fn sanitize_binary_output(text: &str) -> String {
    text.chars()
        .filter(|character| {
            let code = *character as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            true
        })
        .collect()
}

/// Port of `trimToLastUtf8Bytes`.
fn trim_to_last_utf8_bytes(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[derive(Clone)]
enum FullOutputWrite {
    Create { initial: String },
    Append { text: String },
}

#[derive(Default, Clone)]
struct CaptureState {
    tail_output: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_output_path: Option<String>,
    full_output_requested: bool,
    pending_writes: VecDeque<FullOutputWrite>,
}

impl CaptureState {
    fn create_progress(&self) -> ShellCaptureProgress {
        let tail_truncation = truncate_tail(&self.tail_output, Default::default());
        let total_lines = self.completed_lines + usize::from(self.has_open_line);
        let truncated = total_lines > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES;
        let truncation = TruncationResult {
            truncated_by: if truncated {
                Some(tail_truncation.truncated_by.unwrap_or(
                    if self.total_bytes > DEFAULT_MAX_BYTES {
                        TruncatedBy::Bytes
                    } else {
                        TruncatedBy::Lines
                    },
                ))
            } else {
                None
            },
            truncated,
            total_lines,
            total_bytes: self.total_bytes,
            ..tail_truncation
        };
        ShellCaptureProgress {
            output: if truncated {
                truncation.content.clone()
            } else {
                self.tail_output.clone()
            },
            truncation,
            full_output_path: self.full_output_path.clone(),
            last_line_bytes: self.current_line_bytes,
        }
    }

    fn ensure_full_output_file(&mut self, initial: String) {
        if self.full_output_requested {
            return;
        }
        self.full_output_requested = true;
        self.pending_writes
            .push_back(FullOutputWrite::Create { initial });
    }

    fn append_full_output(&mut self, text: &str) {
        if !self.full_output_requested {
            return;
        }
        self.pending_writes.push_back(FullOutputWrite::Append {
            text: text.to_string(),
        });
    }

    fn on_chunk(&mut self, chunk: &str) -> String {
        let text = sanitize_binary_output(chunk).replace('\r', "");
        let text_bytes = text.len();
        self.total_bytes += text_bytes;
        let newline_count = text.matches('\n').count();
        self.completed_lines += newline_count;
        if let Some(last_newline) = text.rfind('\n') {
            let trailing_text = &text[last_newline + 1..];
            self.current_line_bytes = trailing_text.len();
            self.has_open_line = !trailing_text.is_empty();
        } else if !text.is_empty() {
            self.current_line_bytes += text_bytes;
            self.has_open_line = true;
        }

        self.tail_output.push_str(&text);
        let total_lines = self.completed_lines + usize::from(self.has_open_line);
        if (self.total_bytes > DEFAULT_MAX_BYTES || total_lines > DEFAULT_MAX_LINES)
            && !self.full_output_requested
        {
            let initial = self.tail_output.clone();
            self.ensure_full_output_file(initial);
        } else if self.full_output_requested {
            self.append_full_output(&text);
        }
        let max_output_bytes = DEFAULT_MAX_BYTES * 2;
        self.tail_output = trim_to_last_utf8_bytes(&self.tail_output, max_output_bytes);
        text
    }
}

fn to_execution_error(error: &super::super::types::FileError) -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string())
}

/// Port of `executeShellWithCapture`.
pub async fn execute_shell_with_capture(
    env: &dyn ExecutionEnv,
    command: &str,
    options: Option<ShellCaptureOptions>,
) -> Result<ShellCaptureResult, ExecutionError> {
    let options = options.unwrap_or_default();

    // The stdout/stderr listeners process chunks through the shared
    // capture state and then forward the sanitized text plus the progress
    // snapshot to the caller's onChunk callback.
    let shared_state: Arc<Mutex<CaptureState>> = Arc::new(Mutex::new(CaptureState::default()));
    let on_chunk_listener: super::super::types::ChunkListener = {
        let shared_state = Arc::clone(&shared_state);
        let forward = options.on_chunk.clone();
        Arc::new(move |chunk: &str| {
            let mut state = shared_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let text = state.on_chunk(chunk);
            if let Some(forward) = &forward {
                forward(&text, state.create_progress());
            }
            Ok(())
        })
    };
    let exec_options = ShellExecOptions {
        cwd: options.cwd.clone(),
        env: options.env.clone(),
        inherit_env: options.inherit_env,
        timeout: options.timeout,
        abort_signal: options.abort_signal.clone(),
        on_stdout: Some(Arc::clone(&on_chunk_listener)),
        on_stderr: Some(on_chunk_listener),
    };

    let result = env.exec(command, Some(&exec_options)).await;

    // Snapshot the capture state; no listener runs after exec settles.
    let mut state = shared_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let progress = state.create_progress();
    if progress.truncation.truncated && !state.full_output_requested {
        state.ensure_full_output_file(state.tail_output.clone());
    }
    // Process the queued full-output writes sequentially (the TypeScript
    // write chain); the first failure short-circuits like the chained
    // promises.
    while let Some(write) = state.pending_writes.pop_front() {
        match write {
            FullOutputWrite::Create { initial } => {
                let temp_file = env
                    .create_temp_file(
                        &super::super::types::CreateTempFileOptions {
                            prefix: Some("bash-".to_string()),
                            suffix: Some(".log".to_string()),
                        },
                        options.abort_signal.clone(),
                    )
                    .await;
                match temp_file {
                    Ok(path) => {
                        state.full_output_path = Some(path.clone());
                        if let Err(error) = env
                            .append_file(&path, &WriteContent::Text(initial), None)
                            .await
                        {
                            return Err(to_execution_error(&error));
                        }
                    }
                    Err(error) => return Err(to_execution_error(&error)),
                }
            }
            FullOutputWrite::Append { text } => {
                let Some(path) = state.full_output_path.clone() else {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::Unknown,
                        "Full output path was not created",
                    ));
                };
                if let Err(error) = env
                    .append_file(&path, &WriteContent::Text(text), None)
                    .await
                {
                    return Err(to_execution_error(&error));
                }
            }
        }
    }
    let progress = state.create_progress();

    match result {
        Err(error) => {
            if error.code == ExecutionErrorCode::Aborted
                || options
                    .abort_signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_cancelled())
            {
                return Ok(ShellCaptureResult {
                    exit_code: None,
                    cancelled: true,
                    execution_error: None,
                    truncated: progress.truncation.truncated,
                    output: progress.output,
                    truncation: progress.truncation,
                    full_output_path: progress.full_output_path,
                    last_line_bytes: progress.last_line_bytes,
                });
            }
            if options.return_execution_errors.unwrap_or(false) {
                return Ok(ShellCaptureResult {
                    exit_code: None,
                    cancelled: false,
                    execution_error: Some(error),
                    truncated: progress.truncation.truncated,
                    output: progress.output,
                    truncation: progress.truncation,
                    full_output_path: progress.full_output_path,
                    last_line_bytes: progress.last_line_bytes,
                });
            }
            Err(error)
        }
        Ok(result) => {
            let cancelled = options
                .abort_signal
                .as_ref()
                .is_some_and(|signal| signal.is_cancelled());
            Ok(ShellCaptureResult {
                exit_code: if cancelled {
                    None
                } else {
                    Some(result.exit_code)
                },
                cancelled,
                execution_error: None,
                truncated: progress.truncation.truncated,
                output: progress.output,
                truncation: progress.truncation,
                full_output_path: progress.full_output_path,
                last_line_bytes: progress.last_line_bytes,
            })
        }
    }
}
