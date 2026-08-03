use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable transport-independent execution error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    InvalidSpec,
    UnsupportedBackend,
    ResourceConflict,
    ResourceExhausted,
    BackendFailed,
    Internal,
}

/// An execution failure safe to project through daemon, CLI, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{code:?}: {detail}")]
pub struct ExecutionError {
    pub code: ErrorCode,
    pub retryable: bool,
    pub detail: String,
}

impl ExecutionError {
    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidSpec,
            retryable: false,
            detail: detail.into(),
        }
    }

    pub(crate) fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::UnsupportedBackend,
            retryable: false,
            detail: detail.into(),
        }
    }

    pub(crate) fn backend(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::BackendFailed,
            retryable: false,
            detail: detail.into(),
        }
    }

    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Internal,
            retryable: false,
            detail: detail.into(),
        }
    }
}
