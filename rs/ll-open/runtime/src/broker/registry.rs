//! The endpoint registry (ADR-0046 S4, "Endpoint registry").
//!
//! A 0700 parent and a uid check stop other users, not other runs: every run's
//! VMM, and on native macOS every workload, runs as the daemon's uid. So the
//! daemon keeps, under one lock, every live or starting run's broker endpoints
//! and every confinement §6 path (and directory grant) each run's manifest
//! names, and refuses whichever run registers second when the two overlap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

use crate::ExecutionError;

/// One confinement §6 grant a run's manifest names. A path ending in `/` is a
/// directory grant and covers every path beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketGrant(pub String);

impl SocketGrant {
    fn covers(&self, endpoint: &Path) -> bool {
        let grant = self.0.as_str();
        if let Some(directory) = grant.strip_suffix('/') {
            endpoint.starts_with(Path::new(if directory.is_empty() {
                "/"
            } else {
                directory
            }))
        } else {
            Path::new(grant) == endpoint
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Entry {
    endpoints: Vec<PathBuf>,
    grants: Vec<SocketGrant>,
}

/// Every live or starting run's endpoints and §6 grants, under one lock.
#[derive(Debug, Default)]
pub struct EndpointRegistry {
    runs: Mutex<BTreeMap<String, Entry>>,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a run's broker endpoints and its manifest's §6 grants, or
    /// refuse it. A run with no connections registers its grants too, so a
    /// later run's endpoint cannot land under one of them.
    pub fn register(
        &self,
        run_id: &str,
        endpoints: &[PathBuf],
        grants: &[SocketGrant],
    ) -> Result<(), ExecutionError> {
        let mut runs = self.runs.lock();
        if runs.contains_key(run_id) {
            return Err(ExecutionError::resource_conflict(format!(
                "run {run_id} is already registered"
            )));
        }
        for (other_id, other) in runs.iter() {
            for grant in grants {
                if let Some(endpoint) = other.endpoints.iter().find(|e| grant.covers(e)) {
                    return Err(ExecutionError::invalid_grant(format!(
                        "confinement §6 grant {} reaches {}, the broker endpoint of another run ({other_id})",
                        grant.0,
                        endpoint.display()
                    )));
                }
            }
            for endpoint in endpoints {
                if other.endpoints.contains(endpoint)
                    || other.grants.iter().any(|g| g.covers(endpoint))
                {
                    return Err(ExecutionError::resource_conflict(format!(
                        "endpoint {}: endpoint held by another run ({other_id})",
                        endpoint.display()
                    )));
                }
            }
        }
        runs.insert(
            run_id.to_owned(),
            Entry {
                endpoints: endpoints.to_vec(),
                grants: grants.to_vec(),
            },
        );
        Ok(())
    }

    /// Remove a run once its broker is gone and its endpoints are unlinked.
    /// On the failed-cancel path the caller keeps the entry until cleanup's
    /// cancel succeeds (S7).
    pub fn remove(&self, run_id: &str) -> bool {
        self.runs.lock().remove(run_id).is_some()
    }

    pub fn contains(&self, run_id: &str) -> bool {
        self.runs.lock().contains_key(run_id)
    }
}
