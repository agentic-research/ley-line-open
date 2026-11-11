//! A run's workspace shares, materialized by the service before a backend
//! starts anything (bead `ley-line-open-1022a8`).
//!
//! Workspace authority is passed as a capability. [`ExecutionService`] opens
//! each workspace a verified grant names and hands the open shares to
//! [`Backend::start`] as [`ServedWorkspaces`]. A backend cannot construct
//! that value, so it can only start a run with the shares it was handed; and
//! it cannot claim a share it did not take, because the claim
//! ([`AttachedWorkspace`]) exists only as the result of taking one
//! ([`ServedWorkspace::attach`]).
//!
//! [`ExecutionService`]: crate::ExecutionService
//! [`Backend::start`]: crate::Backend::start

use std::sync::Arc;

use crate::backends::libkrun::share::{FsService, WorkspaceShares};
use crate::{ExecutionError, WorkerChannel, WorkspaceGrant};

/// The workspace shares of one run: one open share per workspace its grant
/// names, in grant order.
///
/// Only the service constructs a non-empty value, from a verified grant. A
/// backend receives it and cannot build one:
///
/// ```compile_fail,E0451
/// let forged = leyline_runtime::ServedWorkspaces { shares: Vec::new() };
/// ```
///
/// The empty set is not authority, so anyone may state it
/// ([`ServedWorkspaces::none`]): it is what a direct `Backend::start`
/// embedder passes, and a backend refuses a request that names workspaces
/// beside it.
pub struct ServedWorkspaces {
    shares: Vec<ServedWorkspace>,
}

/// One granted workspace, open: the grant and the service answering the
/// guest's requests for it, scoped by the grant.
pub struct ServedWorkspace {
    grant: WorkspaceGrant,
    service: Arc<dyn FsService>,
}

/// Proof that a backend took a [`ServedWorkspace`] to attach to its worker.
/// The run's receipt names the share from this value, never from anything a
/// backend writes itself:
///
/// ```compile_fail,E0451
/// let forged = leyline_runtime::AttachedWorkspace {
///     workspace: "repo".into(),
///     graph_root: String::new(),
/// };
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct AttachedWorkspace {
    workspace: String,
    graph_root: String,
}

impl ServedWorkspaces {
    /// No workspaces: a run granted none.
    pub fn none() -> Self {
        Self { shares: Vec::new() }
    }

    /// Open every workspace in `grants` from `provider`, for run `run_id`.
    /// Refused as `workspaceUnserved` when there are grants and no provider;
    /// a provider's own refusal (a graph root the store does not hold, say)
    /// is returned as it is. Nothing has been spawned when this fails.
    pub(crate) fn materialize(
        provider: Option<&dyn WorkspaceShares>,
        run_id: &str,
        grants: &[WorkspaceGrant],
    ) -> Result<Self, ExecutionError> {
        let Some(first) = grants.first() else {
            return Ok(Self::none());
        };
        let provider = provider.ok_or_else(|| {
            ExecutionError::workspace_unserved(format!(
                "the run is granted workspace {:?}, and this execution service serves \
                 no workspace shares (start the daemon with a source tree)",
                first.name
            ))
        })?;
        let shares = grants
            .iter()
            .map(|grant| {
                Ok(ServedWorkspace {
                    grant: grant.clone(),
                    service: provider.open(run_id, grant)?,
                })
            })
            .collect::<Result<_, ExecutionError>>()?;
        Ok(Self { shares })
    }

    /// As the service materializes them, for a test that drives a backend
    /// directly. `test-support` only: no shipping build can construct shares
    /// outside the service (tools/feature-ledger.txt).
    #[cfg(feature = "test-support")]
    pub fn materialize_unverified(
        provider: &dyn WorkspaceShares,
        run_id: &str,
        grants: &[WorkspaceGrant],
    ) -> Result<Self, ExecutionError> {
        Self::materialize(Some(provider), run_id, grants)
    }

    pub fn is_empty(&self) -> bool {
        self.shares.is_empty()
    }

    pub fn len(&self) -> usize {
        self.shares.len()
    }

    /// The grants these shares serve, in order.
    pub fn grants(&self) -> impl Iterator<Item = &WorkspaceGrant> {
        self.shares.iter().map(|share| &share.grant)
    }

    /// Whether these shares serve exactly `grants`, each once, in order: the
    /// check a backend makes before it spawns, so the request its worker is
    /// sent names the workspaces it is handed and no others.
    pub fn serve_exactly(&self, grants: &[WorkspaceGrant]) -> bool {
        self.grants().eq(grants.iter())
    }

    /// The shares, for a backend to attach.
    pub fn into_shares(self) -> Vec<ServedWorkspace> {
        self.shares
    }
}

impl ServedWorkspace {
    /// The grant this share serves.
    pub fn grant(&self) -> &WorkspaceGrant {
        &self.grant
    }

    /// Take the share to attach to the worker: the service that answers its
    /// requests, and the proof the run's receipt names it from.
    pub fn attach(self) -> (Arc<dyn FsService>, AttachedWorkspace) {
        (
            self.service,
            AttachedWorkspace {
                workspace: self.grant.name,
                graph_root: self.grant.graph_root,
            },
        )
    }
}

impl AttachedWorkspace {
    /// The channel the run's backend evidence names for this share.
    pub fn channel(&self) -> WorkerChannel {
        WorkerChannel::WorkspaceShare {
            workspace: self.workspace.clone(),
            graph_root: self.graph_root.clone(),
        }
    }
}
