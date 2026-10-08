//! Port of `pi-core/agent/src/harness/session/jsonl/errors.ts`.

use crate::agent::harness::types::{FileError, FileErrorCode};

use super::super::types::{SessionError, SessionErrorCode};

/// Port of `JsonlDecodeError`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonlDecodeError {
    pub kind: JsonlDecodeErrorKind,
    pub message: String,
}

/// The decode failure discriminants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonlDecodeErrorKind {
    Syntax,
    Schema,
}

impl JsonlDecodeError {
    pub fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeErrorKind::Syntax,
            message: message.into(),
        }
    }

    pub fn schema(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeErrorKind::Schema,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JsonlDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for JsonlDecodeError {}

/// Port of `fileResult`: maps filesystem failures onto `SessionError`.
pub fn file_result<T>(result: Result<T, FileError>, message: &str) -> Result<T, SessionError> {
    result.map_err(|error| SessionError {
        code: if error.code == FileErrorCode::NotFound {
            SessionErrorCode::NotFound
        } else {
            SessionErrorCode::Storage
        },
        message: format!("{message}: {}", error.message),
    })
}

/// Port of `invalidFile`.
pub fn invalid_file(path: &str, line: usize, cause: &JsonlDecodeError) -> SessionError {
    SessionError::new(
        SessionErrorCode::InvalidEntry,
        format!(
            "Invalid JSONL v4 session {path}: line {line} {}",
            cause.message
        ),
    )
}

/// `file_result` for operations whose success value is unit; returns the
/// mapped error directly.
pub fn file_err(error: FileError, message: &str) -> SessionError {
    file_result::<()>(Err(error), message).unwrap_err()
}
