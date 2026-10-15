//! The daemon's side of a run's workspace share (ADR-0044 V5, bead
//! `ley-line-open-f210c2`).
//!
//! The libkrun backend asks [`DaemonShares`] for the run's granted
//! workspace before it spawns the VMM worker, and answers the worker's
//! share requests with the [`StoreShare`] it returns. That share is a
//! [`StoreFs`] over the daemon's own context: reads on its reader pool,
//! proposals on its one writer, each landed proposal written into its
//! checkout and published to its arena, as the socket's `propose` op does.

use std::collections::BTreeSet;
use std::sync::Arc;

use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
use leyline_cli_lib::daemon::{ContextSlot, DaemonContext};
use leyline_cli_lib::source_write::Grant;
use leyline_cli_lib::tree_root::DerivedFrom;
use leyline_runtime::backends::libkrun::share::{FsService, WorkspaceShares};
use leyline_runtime::{ErrorCode, ExecutionError, WorkspaceGrant, WorkspaceOperation};

use crate::{Checkout, Freshness, Options, Scope, Store, StoreFs, StoreShare};

/// The daemon's living database: the share reads on its reader pool and
/// proposes on its one writer, the same connections the socket's `propose`
/// and `read_source` use.
impl Store for DaemonContext {
    fn with_read<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        DaemonContext::with_read(self, f)
    }

    fn with_write<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        DaemonContext::with_write(self, f)
    }

    /// Snapshot the living database into the arena, on the writer, as the
    /// socket's `propose` op does after its commit.
    fn publish(&self) -> anyhow::Result<()> {
        DaemonContext::with_write(self, |conn| snapshot_to_arena(conn, &self.ctrl_path))
    }
}

/// What a grant's operations let the guest do: `read` and `list` let it see
/// the tree, `mutate` and `commit` let it propose. `query` names no file
/// operation. `execution/v1` grants a workspace whole; it names no paths.
pub fn scope_for(operations: &[WorkspaceOperation]) -> Scope {
    let any = |wanted: [WorkspaceOperation; 2]| {
        operations
            .iter()
            .any(|operation| wanted.contains(operation))
    };
    let whole_if = |granted: bool| {
        if granted {
            Grant::Whole
        } else {
            Grant::Paths(BTreeSet::new())
        }
    };
    Scope {
        see: whole_if(any([WorkspaceOperation::Read, WorkspaceOperation::List])),
        propose: whole_if(any([
            WorkspaceOperation::Mutate,
            WorkspaceOperation::Commit,
        ])),
    }
}

/// Serves each run's granted workspace from the daemon's store.
pub struct DaemonShares {
    daemon: ContextSlot,
    freshness: Freshness,
}

impl DaemonShares {
    /// Shares from the daemon whose context `daemon` will hold once it has
    /// started, answering with `freshness`'s timeouts.
    pub fn new(daemon: ContextSlot, freshness: Freshness) -> Self {
        Self { daemon, freshness }
    }
}

fn refused(code: ErrorCode, retryable: bool, detail: String) -> ExecutionError {
    ExecutionError {
        code,
        retryable,
        detail,
    }
}

impl WorkspaceShares for DaemonShares {
    /// A share of the store, scoped by the grant's operations, when the
    /// store's `treeRoot` (ADR-0040 D1: the tagged fold over every
    /// `_source` row) is the grant's `graphRoot`: the run starts from the
    /// tree it was authorized against. Once open, the share serves the live
    /// store: another writer's proposals are visible at the next open, and
    /// an edit against bytes another writer moved is refused (ADR-0044 V3).
    fn open(
        &self,
        run_id: &str,
        grant: &WorkspaceGrant,
    ) -> Result<Arc<dyn FsService>, ExecutionError> {
        let ctx = self.daemon.get().ok_or_else(|| {
            refused(
                ErrorCode::NotProvisioned,
                true,
                "the daemon serving workspace shares has not started".into(),
            )
        })?;
        let tree_root = Store::with_read(ctx.as_ref(), |conn| {
            Ok(DerivedFrom::read(conn)?.map(|derived| derived.tree_root))
        })
        .map_err(|error| {
            refused(
                ErrorCode::Internal,
                false,
                format!("read the store's tree root: {error:#}"),
            )
        })?;
        let held = tree_root.map(|root| format!("blake3-256:{root}"));
        if held.as_deref() != Some(grant.graph_root.as_str()) {
            return Err(refused(
                ErrorCode::IdentityPolicyMismatch,
                false,
                format!(
                    "workspace {:?} is granted at graph root {}, and the store holds {}",
                    grant.name,
                    grant.graph_root,
                    held.as_deref().unwrap_or("no tree root")
                ),
            ));
        }
        let options = Options {
            freshness: self.freshness,
            checkout: ctx.source_dir.clone().map(|dir| Checkout {
                dir,
                lang_filter: ctx.lang_filter.clone(),
            }),
        };
        log::info!(
            "run {run_id}: serving workspace {:?} at {} ({:?})",
            grant.name,
            grant.graph_root,
            grant.operations
        );
        Ok(Arc::new(StoreShare::new(StoreFs::new(
            Arc::clone(ctx),
            scope_for(&grant.operations),
            options,
        ))))
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
