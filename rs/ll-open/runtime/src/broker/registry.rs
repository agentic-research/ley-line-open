//! The endpoint registry (ADR-0046 S4, "Endpoint registry").
//!
//! A 0700 parent and a uid check stop other users, not other runs: every run's
//! VMM, and on native macOS every workload, runs as the daemon's uid. So the
//! daemon keeps, under one lock, every live or starting run's broker endpoints
//! and every confinement §6 path (and directory grant) each run's manifest
//! names, and refuses whichever run registers second when the two overlap.
//!
//! Paths are compared as the sandbox enforces them, not as they are spelled:
//! nono resolves a §6 grant to its real path (symlinks followed, on-disk case)
//! before Seatbelt or Landlock sees it. So both sides are resolved at the
//! moment of comparison (the longest existing ancestor through `realpath`, the
//! rest appended), and on macOS, whose default volumes are case-insensitive,
//! compared without regard to ASCII case. Over-matching on a case-sensitive
//! volume refuses a run; it never admits one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

use crate::ExecutionError;

/// One confinement §6 grant a run's manifest names. A path ending in `/` is a
/// directory grant and covers every path beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketGrant(pub String);

/// A path as the kernel will resolve it: the longest existing ancestor
/// through `realpath`, the remaining components appended, case-folded on
/// macOS. `.` and `..` components that `realpath` did not consume are kept
/// literally, which can only make two paths compare unequal.
pub fn resolved(path: &Path) -> String {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let base = loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            break real;
        }
        match (
            existing.file_name().map(|n| n.to_owned()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break existing,
        }
    };
    let mut full = base;
    for name in rest.into_iter().rev() {
        full.push(name);
    }
    let text = full.to_string_lossy().into_owned();
    if cfg!(target_os = "macos") {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

impl SocketGrant {
    fn covers(&self, endpoint: &Path) -> bool {
        let endpoint = resolved(endpoint);
        let grant = self.0.as_str();
        if let Some(directory) = grant.strip_suffix('/') {
            let directory = resolved(Path::new(if directory.is_empty() {
                "/"
            } else {
                directory
            }));
            Path::new(&endpoint).starts_with(Path::new(&directory))
        } else {
            resolved(Path::new(grant)) == endpoint
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
                let resolved_endpoint = resolved(endpoint);
                if other
                    .endpoints
                    .iter()
                    .any(|e| resolved(e) == resolved_endpoint)
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
