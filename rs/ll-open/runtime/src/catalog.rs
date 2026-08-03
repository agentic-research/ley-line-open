//! Explicit catalog-backed resolution from schema identities to backend requests.
//!
//! The catalog is deliberately boring: it is an embedding-owned table that
//! binds an authenticated executable artifact and workspace graph roots to a
//! content-addressed rootfs and guest-relative entrypoint. It never accepts or
//! derives host paths from a wire request. Backend implementations resolve the
//! returned rootfs digest beneath their own configured CAS root.

use std::collections::HashMap;

use serde::Deserialize;

use crate::{
    ExecutionError, ExecutionRequest, ExecutionResolver, ResourceLimits,
    authorization::{AuthorizedExecution, SchemaLimits, WorkspaceInput},
    model::DigestRef,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ArtifactKey {
    digest: String,
    media_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogEntry {
    rootfs: DigestRef,
    executable: String,
    workspace_inputs: Vec<WorkspaceInput>,
}

/// Builder for a trusted execution catalog.
#[derive(Debug, Default)]
pub struct CatalogBuilder {
    entries: Vec<(ArtifactKey, CatalogEntry)>,
}

impl CatalogBuilder {
    pub fn entry(
        mut self,
        artifact_digest: impl Into<String>,
        media_type: impl Into<String>,
        rootfs: DigestRef,
        executable: impl Into<String>,
        workspace_inputs: Vec<WorkspaceInput>,
    ) -> Self {
        self.entries.push((
            ArtifactKey {
                digest: artifact_digest.into(),
                media_type: media_type.into(),
            },
            CatalogEntry {
                rootfs,
                executable: executable.into(),
                workspace_inputs,
            },
        ));
        self
    }

    pub fn build(self) -> Result<CatalogResolver, ExecutionError> {
        let mut entries = HashMap::with_capacity(self.entries.len());
        for (key, entry) in self.entries {
            if key.digest.is_empty() || key.media_type.is_empty() {
                return Err(ExecutionError::invalid(
                    "catalog artifact identity must not be empty",
                ));
            }
            entry.rootfs.validate_blake3()?;
            validate_guest_path(&entry.executable)?;
            if entries.insert(key, entry).is_some() {
                return Err(ExecutionError {
                    code: crate::ErrorCode::ResourceConflict,
                    retryable: false,
                    detail: "catalog contains duplicate artifact identity".into(),
                });
            }
        }
        Ok(CatalogResolver { entries })
    }
}

/// Trusted resolver backed by an explicit content-addressed execution catalog.
#[derive(Debug, Clone, Default)]
pub struct CatalogResolver {
    entries: HashMap<ArtifactKey, CatalogEntry>,
}

impl CatalogResolver {
    pub fn builder() -> CatalogBuilder {
        CatalogBuilder::default()
    }

    /// Load the explicit, embedding-owned catalog document used by the
    /// first-party execution daemon. This document is configuration, not a
    /// replacement for the signed execution/v1 wire schema.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ExecutionError> {
        let document: CatalogDocument = serde_json::from_slice(bytes).map_err(|error| {
            ExecutionError::invalid(format!("invalid execution catalog: {error}"))
        })?;
        let mut builder = Self::builder();
        for entry in document.entries {
            builder = builder.entry(
                entry.artifact_digest,
                entry.media_type,
                entry.rootfs,
                entry.executable,
                entry
                    .workspace_inputs
                    .into_iter()
                    .map(|workspace| WorkspaceInput {
                        name: workspace.name,
                        graph_root: workspace.graph_root,
                    })
                    .collect(),
            );
        }
        builder.build()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    entries: Vec<CatalogEntryDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntryDocument {
    #[serde(rename = "artifactDigest")]
    artifact_digest: String,
    #[serde(rename = "mediaType")]
    media_type: String,
    rootfs: DigestRef,
    executable: String,
    #[serde(rename = "workspaceInputs")]
    workspace_inputs: Vec<WorkspaceInputDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceInputDocument {
    name: String,
    #[serde(rename = "graphRoot")]
    graph_root: String,
}

impl ExecutionResolver for CatalogResolver {
    fn resolve(
        &self,
        authorized: &AuthorizedExecution,
    ) -> Result<ExecutionRequest, ExecutionError> {
        let key = ArtifactKey {
            digest: authorized.intent.executable.digest.clone(),
            media_type: authorized.intent.executable.media_type.clone(),
        };
        let entry = self.entries.get(&key).ok_or_else(|| {
            ExecutionError::identity_mismatch(
                "execution artifact is not present in the trusted catalog",
            )
        })?;
        if entry.workspace_inputs != authorized.intent.workspace_inputs {
            return Err(ExecutionError::identity_mismatch(
                "execution workspace identity is not present in the trusted catalog",
            ));
        }
        let limits = resource_limits(authorized.intent.requested_limits)?;
        let request = ExecutionRequest {
            run_id: authorized.run_id.clone(),
            replay_key: authorized.replay_key.clone(),
            rootfs: entry.rootfs.clone(),
            executable: entry.executable.clone(),
            arguments: authorized.intent.arguments.clone(),
            public_environment: authorized.intent.public_environment.clone(),
            allowed_egress: authorized.allowed_egress.clone(),
            limits,
        };
        request.validate()?;
        Ok(request)
    }
}

fn resource_limits(limits: SchemaLimits) -> Result<ResourceLimits, ExecutionError> {
    if limits.output_bytes != 0 {
        return Err(ExecutionError::unsupported(
            "output byte limits are not enforced by the execution backend",
        ));
    }
    // The backend accepts whole vCPUs/MiB. Rounding up would widen the
    // caller's authorized ceiling (e.g. 1001 millis -> 2 vCPUs), so reject
    // values that cannot be represented exactly instead.
    if !limits.cpu_millis.is_multiple_of(1_000) || !limits.memory_bytes.is_multiple_of(1024 * 1024)
    {
        return Err(ExecutionError::invalid(
            "cpu and memory limits must be whole backend units",
        ));
    }
    let vcpus = limits
        .cpu_millis
        .checked_div(1_000)
        .ok_or_else(|| ExecutionError::invalid("cpu limit conversion failed"))?
        .try_into()
        .map_err(|_| ExecutionError::invalid("cpu limit exceeds backend capacity"))?;
    let memory_mib = limits
        .memory_bytes
        .checked_div(1024 * 1024)
        .ok_or_else(|| ExecutionError::invalid("memory limit conversion failed"))?
        .try_into()
        .map_err(|_| ExecutionError::invalid("memory limit exceeds backend capacity"))?;
    if vcpus == 0 || memory_mib == 0 || limits.wall_time_ms == 0 {
        return Err(ExecutionError::invalid(
            "requested execution limits must be non-zero",
        ));
    }
    Ok(ResourceLimits {
        vcpus,
        memory_mib,
        wall_time_ms: limits.wall_time_ms,
    })
}

fn validate_guest_path(value: &str) -> Result<(), ExecutionError> {
    use std::path::{Component, Path};
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ExecutionError::invalid(
            "catalog executable must be a guest-relative path without traversal",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::authorization::{ArtifactIdentity, SchemaIntent};

    #[test]
    fn json_catalog_is_used_to_resolve_a_guest_entrypoint() {
        let artifact = "blake3-256:artifact";
        let json = format!(
            r#"{{"entries":[{{
                "artifactDigest":"{artifact}",
                "mediaType":"application/test-executable",
                "rootfs":{{"algorithm":"blake3-256","value":"{}"}},
                "executable":"bin/agent",
                "workspaceInputs":[]
            }}]}}"#,
            "a".repeat(64)
        );
        let resolver = CatalogResolver::from_json(json.as_bytes()).expect("catalog JSON");
        let authorized = AuthorizedExecution {
            run_id: "run-1".into(),
            grant_id: "grant-1".into(),
            replay_key: "replay-1".into(),
            spec_digest: "blake3-256:spec".into(),
            grant_digest: "blake3-256:grant".into(),
            confinement_digest: "blake3-256:confinement".into(),
            backend: crate::BackendClass::Native,
            allowed_egress: Vec::new(),
            intent: SchemaIntent {
                executable: ArtifactIdentity {
                    digest: artifact.into(),
                    media_type: "application/test-executable".into(),
                },
                arguments: Vec::new(),
                public_environment: BTreeMap::new(),
                workspace_inputs: Vec::new(),
                requested_limits: SchemaLimits {
                    wall_time_ms: 1_000,
                    memory_bytes: 1024 * 1024,
                    cpu_millis: 1_000,
                    output_bytes: 0,
                },
            },
        };
        let request = resolver.resolve(&authorized).expect("resolve catalog");
        assert_eq!(request.executable, "bin/agent");
    }
}
