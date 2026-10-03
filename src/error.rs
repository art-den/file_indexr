//! Domain error at the service boundary (HTTP, MCP).
//!
//! Internal layers keep using `anyhow` for context-rich plumbing errors; at
//! the boundaries (file loading, path validation, transport handlers) the
//! error is converted to [`AppError`] so each transport maps it to its native
//! representation (HTTP status code, MCP `isError` result).

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use crate::config::PathValidateResult;

#[cfg(test)]
#[path = "tests/tests_error.rs"]
mod tests;

/// Domain error shared by the HTTP and MCP transports.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("file not found: {path}")]
    NotFound { path: PathBuf },

    #[error("path is outside the watched directory: {path}")]
    OutsideDirectory { path: PathBuf },

    #[error("not a regular file: {path}")]
    NotAFile { path: PathBuf },

    #[error("Unsupported format: {extension}")]
    UnsupportedFormat { extension: String },

    #[error("file is too large: {size} bytes (limit {limit} bytes)")]
    TooLarge { size: u64, limit: u64 },

    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to convert {path}: {detail}")]
    Conversion { path: PathBuf, detail: String },
}

impl AppError {
    /// HTTP status code for the web transport.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::OutsideDirectory { .. } => StatusCode::FORBIDDEN,
            Self::NotAFile { .. } => StatusCode::BAD_REQUEST,
            Self::UnsupportedFormat { .. } => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Io { .. } | Self::Conversion { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Map a filesystem error to the domain error (NotFound vs generic I/O).
    pub fn from_io(error: std::io::Error, path: &Path) -> AppError {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            AppError::Io {
                path: path.to_path_buf(),
                source: error,
            }
        }
    }

    /// Error for a file that exceeds the configured size limit.
    pub fn too_large(size: u64, limit: u64) -> AppError {
        AppError::TooLarge { size, limit }
    }

    /// Error for a path without a supported file format.
    pub fn unsupported_format(path: &Path) -> AppError {
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "(no extension)".to_string());
        AppError::UnsupportedFormat { extension }
    }

    /// Map a failed path validation to an error. Only non-`Valid` outcomes
    /// are valid input; the `Valid` variant must be handled by the caller.
    pub fn from_invalid_path(path: &str, result: PathValidateResult) -> AppError {
        match result {
            PathValidateResult::Valid(_) => unreachable!("called with a valid path"),
            PathValidateResult::OutsideDirectory => AppError::OutsideDirectory {
                path: path.to_string().into(),
            },
            PathValidateResult::NotFound => AppError::NotFound {
                path: path.to_string().into(),
            },
        }
    }
}
