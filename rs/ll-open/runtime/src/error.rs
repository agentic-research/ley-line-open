use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable transport-independent execution error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    InvalidSpec,
    InvalidGrant,
    Unauthenticated,
    Unauthorized,
    IdentityPolicyMismatch,
    UnsupportedBackend,
    NotProvisioned,
    ResourceConflict,
    ResourceExhausted,
    BackendFailed,
    Cancelled,
    Internal,
    /// The run is granted a workspace the selected backend cannot serve:
    /// it has no workspace share to deliver it through, or more workspaces
    /// than it attaches. Not retryable under the same grant; a backend that
    /// serves workspace shares may run it (bead `ley-line-open-204a94`).
    WorkspaceUnserved,
}

impl ErrorCode {
    /// Every code, in the order of `execution.capnp`'s `ErrorCode` ordinals.
    pub const ALL: [ErrorCode; 13] = [
        ErrorCode::InvalidSpec,
        ErrorCode::InvalidGrant,
        ErrorCode::Unauthenticated,
        ErrorCode::Unauthorized,
        ErrorCode::IdentityPolicyMismatch,
        ErrorCode::UnsupportedBackend,
        ErrorCode::NotProvisioned,
        ErrorCode::ResourceConflict,
        ErrorCode::ResourceExhausted,
        ErrorCode::BackendFailed,
        ErrorCode::Cancelled,
        ErrorCode::Internal,
        ErrorCode::WorkspaceUnserved,
    ];

    /// The code as the execution/v1 schema spells it on the wire: the
    /// `ErrorCode` enumerant of `execution.capnp` (camelCase), which every
    /// transport carries in its structured error (bead
    /// `ley-line-open-e347ae`). Not the serde spelling (kebab-case) and not
    /// the `Debug` one that prefixes [`ExecutionError`]'s message.
    pub fn wire_name(self) -> &'static str {
        match self {
            ErrorCode::InvalidSpec => "invalidSpec",
            ErrorCode::InvalidGrant => "invalidGrant",
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::IdentityPolicyMismatch => "identityPolicyMismatch",
            ErrorCode::UnsupportedBackend => "unsupportedBackend",
            ErrorCode::NotProvisioned => "notProvisioned",
            ErrorCode::ResourceConflict => "resourceConflict",
            ErrorCode::ResourceExhausted => "resourceExhausted",
            ErrorCode::BackendFailed => "backendFailed",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Internal => "internal",
            ErrorCode::WorkspaceUnserved => "workspaceUnserved",
        }
    }
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
    /// A malformed or refused request ([`ErrorCode::InvalidSpec`]).
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidSpec,
            retryable: false,
            detail: detail.into(),
        }
    }

    /// Authority that is malformed or inconsistent ([`ErrorCode::InvalidGrant`]).
    pub(crate) fn invalid_grant(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidGrant,
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

    /// A granted workspace the backend cannot serve
    /// ([`ErrorCode::WorkspaceUnserved`]).
    pub(crate) fn workspace_unserved(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::WorkspaceUnserved,
            retryable: false,
            detail: detail.into(),
        }
    }

    pub(crate) fn identity_mismatch(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::IdentityPolicyMismatch,
            retryable: false,
            detail: detail.into(),
        }
    }

    pub(crate) fn not_provisioned(detail: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::NotProvisioned,
            retryable: true,
            detail: detail.into(),
        }
    }

    /// A backend that failed while running ([`ErrorCode::BackendFailed`]).
    pub fn backend(detail: impl Into<String>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The enumerant names of a generated Cap'n Proto enum, by ordinal.
    fn enumerants<T: capnp::introspect::Introspect>() -> Vec<String> {
        let capnp::introspect::TypeVariant::Enum(raw) = T::introspect().which() else {
            panic!("not an enum");
        };
        capnp::schema::EnumSchema::new(raw)
            .get_enumerants()
            .expect("enumerants")
            .into_iter()
            .map(|e| {
                e.get_proto()
                    .get_name()
                    .expect("name")
                    .to_str()
                    .expect("UTF-8")
                    .to_owned()
            })
            .collect()
    }

    /// `wire_name` is `execution.capnp`'s `ErrorCode`, ordinal for ordinal,
    /// and covers every code (bead `ley-line-open-e347ae`).
    #[test]
    fn wire_names_are_the_schemas_error_codes() {
        let schema = enumerants::<leyline_public_schema::execution_capnp::ErrorCode>();
        let ours: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.wire_name()).collect();
        assert_eq!(ours, schema);
    }
}
