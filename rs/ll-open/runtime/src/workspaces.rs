//! A run's workspace shares, materialized by the service before a backend
//! starts anything (bead `ley-line-open-1022a8`).
//!
//! Workspace authority is passed as a capability. [`ExecutionService`] opens
//! each workspace a verified grant names and hands the open shares to
//! [`Backend::start`] as [`ServedWorkspaces`]. A backend cannot construct
//! that value, so it can only start a run with shares the service opened; and
//! it cannot claim a share it did not take, because the claim
//! ([`AttachedWorkspace`]) exists only as the result of taking one
//! ([`ServedWorkspace::attach`]).
//!
//! What the service then holds a backend to, when `start` returns:
//!
//! - every proof names this run and one of its grants, whole (name, graph
//!   root, operations, write scope), each grant once. A share opened for
//!   another run, or for the same workspace under a wider grant, is refused;
//! - the share each proof stands for is still held by something the backend
//!   gave it to. A backend that took a share and let it go has no share
//!   serving its worker.
//!
//! What this does not prove: that the worker's own descriptor is wired to
//! the share. That is the backend's contract with its worker (the libkrun
//! tier holds its worker's readiness declaration to the descriptor it
//! handed over).
//!
//! [`ExecutionService`]: crate::ExecutionService
//! [`Backend::start`]: crate::Backend::start

use std::sync::{Arc, Weak};

use crate::backends::libkrun::share::{FsService, Reply, Request, WorkspaceShares};
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

/// One granted workspace, open for one run: the grant and the service
/// answering the guest's requests for it, scoped by the grant.
pub struct ServedWorkspace {
    run_id: String,
    grant: WorkspaceGrant,
    service: Arc<Held>,
}

/// The share as the service hands it out: the provider's service behind one
/// allocation only this run's holders keep alive, so the service can tell
/// whether the share it opened is still held ([`Opened::held`]), whatever
/// the provider itself keeps.
struct Held(Arc<dyn FsService>);

impl FsService for Held {
    fn call(&self, request: Request) -> Reply {
        self.0.call(request)
    }
}

/// Proof that a backend took a [`ServedWorkspace`] to attach to its worker:
/// the run it was opened for and the grant it serves. The run's receipt
/// names the share from this value, never from anything a backend writes
/// itself:
///
/// ```compile_fail,E0451
/// let forged = leyline_runtime::AttachedWorkspace {
///     run_id: String::new(),
///     grant: unimplemented!(),
/// };
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct AttachedWorkspace {
    run_id: String,
    grant: WorkspaceGrant,
}

/// The service's own record of the shares it opened for one run, kept
/// across `Backend::start` to hold the backend's answer to.
pub(crate) struct Opened {
    run_id: String,
    shares: Vec<(WorkspaceGrant, Weak<Held>)>,
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
    ) -> Result<(Self, Opened), ExecutionError> {
        let mut opened = Opened {
            run_id: run_id.to_owned(),
            shares: Vec::new(),
        };
        let Some(first) = grants.first() else {
            return Ok((Self::none(), opened));
        };
        let provider = provider.ok_or_else(|| {
            ExecutionError::workspace_unserved(format!(
                "the run is granted workspace {:?}, and this execution service serves \
                 no workspace shares (start the daemon with a source tree)",
                first.name
            ))
        })?;
        let mut shares = Vec::new();
        for grant in grants {
            let service = Arc::new(Held(provider.open(run_id, grant)?));
            opened
                .shares
                .push((grant.clone(), Arc::downgrade(&service)));
            shares.push(ServedWorkspace {
                run_id: run_id.to_owned(),
                grant: grant.clone(),
                service,
            });
        }
        Ok((Self { shares }, opened))
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
        Self::materialize(Some(provider), run_id, grants).map(|(shares, _)| shares)
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

    /// Whether these shares were opened for `request`: for its run, and for
    /// exactly its workspaces, each once, in order. The check a backend
    /// makes before it spawns, so the request its worker is sent names the
    /// workspaces it is handed and no others.
    pub fn serve_exactly(&self, request: &crate::ExecutionRequest) -> bool {
        self.shares
            .iter()
            .all(|share| share.run_id == request.run_id)
            && self.grants().eq(request.workspaces.iter())
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

    /// The run this share was opened for.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Take the share to attach to the worker: the service that answers its
    /// requests, and the proof the run's receipt names it from. The service
    /// must stay held for as long as the worker may use the share: the
    /// execution service refuses a start whose share nothing holds.
    pub fn attach(self) -> (Arc<dyn FsService>, AttachedWorkspace) {
        (
            self.service,
            AttachedWorkspace {
                run_id: self.run_id,
                grant: self.grant,
            },
        )
    }
}

impl AttachedWorkspace {
    /// The channel the run's backend evidence names for this share.
    pub fn channel(&self) -> WorkerChannel {
        WorkerChannel::WorkspaceShare {
            workspace: self.grant.name.clone(),
            graph_root: self.grant.graph_root.clone(),
        }
    }
}

impl Opened {
    /// The assertion the service makes of a backend's answer: `attached` is
    /// exactly the shares opened for this run, each once, and each is still
    /// held. `workspaceUnserved` otherwise, naming the workspace.
    pub(crate) fn held(&self, attached: &[AttachedWorkspace]) -> Result<(), ExecutionError> {
        for proof in attached {
            let ours = proof.run_id == self.run_id
                && self.shares.iter().any(|(grant, _)| *grant == proof.grant);
            if !ours {
                return Err(ExecutionError::workspace_unserved(format!(
                    "the backend attached a share of workspace {:?} at {} opened for run \
                     {:?} under another grant than this run's; the start is refused",
                    proof.grant.name, proof.grant.graph_root, proof.run_id
                )));
            }
        }
        for (grant, share) in &self.shares {
            let taken = attached
                .iter()
                .filter(|proof| proof.grant == *grant)
                .count();
            if taken == 0 {
                return Err(ExecutionError::workspace_unserved(format!(
                    "the backend started the run without serving its granted workspace \
                     {:?} at {}; the start is refused",
                    grant.name, grant.graph_root
                )));
            }
            if taken > 1 || attached.len() != self.shares.len() {
                return Err(ExecutionError::workspace_unserved(format!(
                    "the backend's workspace shares do not match the run's granted \
                     workspaces one for one (workspace {:?}); the start is refused",
                    grant.name
                )));
            }
            if share.strong_count() == 0 {
                return Err(ExecutionError::workspace_unserved(format!(
                    "the backend took the share of workspace {:?} and nothing holds it, \
                     so it is no longer served to the worker; the start is refused",
                    grant.name
                )));
            }
        }
        Ok(())
    }
}
