//! Schema-backed authorization for execution/v1.
//!
//! `RunSpec` is caller-controlled intent.  `RunGrant` is the only object that
//! can turn that intent into an accepted run.  This module deliberately reads
//! the generated Cap'n Proto surface from `leyline-public-schema`; it does not
//! define a second wire model.  Backend/rootfs resolution happens after this
//! check, through a trusted resolver owned by the runtime.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use capnp::message::ReaderOptions;
use leyline_core::ContentAddressed;
use leyline_envelope::{Envelope, VerifyingKey};
use leyline_public_schema::execution_capnp;

use crate::{BackendClass, ExecutionError};

pub const EXECUTION_SCHEMA_VERSION: &str = "cloister/execution/v1";
pub const EXECUTION_CAPABILITY: &str = "urn:signet:cap:execute:run";
const APAS_PREDICATE_TYPE: &str = "https://rosary.dev/Handoff/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationPolicy {
    pub now_unix_ms: u64,
    pub required_backend: BackendClass,
    /// Digest of the confinement policy the selected backend will enforce.
    /// A grant for any other policy is rejected before resolver invocation.
    pub required_confinement_digest: Option<String>,
}

/// A trust-domain adapter supplied by the embedding authority (Cloister /
/// Interlace). LLO deliberately does not own Signet/NotMe trust roots.
///
/// `EvidenceRef` is a CAS reference, not proof by itself. An adapter must
/// resolve the referenced canonical bytes and verify the appropriate signed
/// envelope/certificate chain before authorization can be called with it.
pub trait EvidenceVerifier: Send + Sync {
    fn verify(&self, field: &str, evidence: &EvidenceRef) -> Result<(), ExecutionError>;
}

/// Minimal CAS interface needed by the built-in APAS DSSE verifier. Cloister
/// can provide its mmap/SQLite CAS adapter without giving LLO host paths.
pub trait EvidenceStore: Send + Sync {
    fn load(&self, digest: &str) -> Result<Vec<u8>, ExecutionError>;
}

/// Verifies APAS/in-toto DSSE evidence from a content-addressed store against
/// embedding-provided trust keys. Key distribution/rotation remains owned by
/// Signet/NotMe; this type only performs cryptographic verification and digest
/// binding once Cloister supplies the active keys.
pub struct CasDsseEvidenceVerifier<S> {
    store: std::sync::Arc<S>,
    trust_keys: Vec<VerifyingKey>,
}

impl<S: EvidenceStore> CasDsseEvidenceVerifier<S> {
    pub fn new(store: std::sync::Arc<S>, trust_keys: Vec<VerifyingKey>) -> Self {
        Self { store, trust_keys }
    }
}

impl<S: EvidenceStore> EvidenceVerifier for CasDsseEvidenceVerifier<S> {
    fn verify(&self, field: &str, evidence: &EvidenceRef) -> Result<(), ExecutionError> {
        if evidence.media_type != "application/vnd.in-toto+json" {
            return Err(ExecutionError::unsupported(format!(
                "{field} is not an APAS in-toto evidence envelope"
            )));
        }
        let bytes = self.store.load(&evidence.digest)?;
        let observed = format!("blake3-256:{}", bytes.hash());
        if observed != evidence.digest {
            return Err(ExecutionError {
                code: crate::ErrorCode::Unauthenticated,
                retryable: false,
                detail: format!("{field} CAS digest does not match evidence bytes"),
            });
        }
        let envelope = Envelope::from_json_slice(&bytes).map_err(|error| ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail: format!("{field} DSSE envelope is invalid: {error}"),
        })?;
        if self.trust_keys.is_empty() {
            return Err(ExecutionError {
                code: crate::ErrorCode::Unauthenticated,
                retryable: false,
                detail: format!("{field} DSSE signature is not trusted"),
            });
        }
        let statement = self
            .trust_keys
            .iter()
            .find_map(|key| envelope.verify(key).ok())
            .ok_or_else(|| ExecutionError {
                code: crate::ErrorCode::Unauthenticated,
                retryable: false,
                detail: format!("{field} DSSE signature is not trusted"),
            })?;
        if statement.predicate_type() != APAS_PREDICATE_TYPE {
            return Err(ExecutionError {
                code: crate::ErrorCode::Unauthenticated,
                retryable: false,
                detail: format!("{field} DSSE predicate is not APAS Handoff/v1"),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub media_type: String,
    pub digest: String,
}

/// Compatibility verifier for existing unit fixtures only. Production
/// integrations must use an embedding-owned verifier and call
/// [`authorize_with_verifier`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MetadataOnlyEvidenceVerifier;

impl EvidenceVerifier for MetadataOnlyEvidenceVerifier {
    fn verify(&self, _field: &str, _evidence: &EvidenceRef) -> Result<(), ExecutionError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RejectUnverifiedEvidence;

impl EvidenceVerifier for RejectUnverifiedEvidence {
    fn verify(&self, field: &str, _evidence: &EvidenceRef) -> Result<(), ExecutionError> {
        Err(ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail: format!("no trusted verifier configured for {field}"),
        })
    }
}

impl Default for AuthorizationPolicy {
    fn default() -> Self {
        Self {
            now_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedExecution {
    /// Generated from the bound spec/grant, never accepted from the caller.
    pub run_id: String,
    pub grant_id: String,
    pub replay_key: String,
    pub spec_digest: String,
    pub grant_digest: String,
    pub confinement_digest: String,
    pub backend: BackendClass,
    pub allowed_egress: Vec<String>,
    pub intent: SchemaIntent,
}

/// Owned, policy-free intent extracted from the generated `RunSpec` reader.
///
/// A resolver may use these logical identities to select a content-addressed
/// rootfs and guest-relative executable. It must not accept host paths from a
/// caller or skip the preceding [`authorize`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIntent {
    pub executable: ArtifactIdentity,
    pub arguments: Vec<String>,
    pub public_environment: BTreeMap<String, String>,
    pub workspace_inputs: Vec<WorkspaceInput>,
    pub requested_limits: SchemaLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    pub digest: String,
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInput {
    pub name: String,
    pub graph_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLimits {
    pub wall_time_ms: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub output_bytes: u64,
}

/// Validate and bind serialized execution/v1 `RunSpec` and `RunGrant` values.
///
/// The digest is over the exact Cap'n Proto message bytes received by this
/// boundary. Producers must use the schema's canonical serialization when
/// constructing the grant; accepting a digest for a different byte sequence is
/// rejected. A later resolver may materialize logical artifacts and Graph
/// roots, but it cannot bypass this authorization step.
pub fn authorize(
    spec_bytes: &[u8],
    grant_bytes: &[u8],
    policy: &AuthorizationPolicy,
) -> Result<AuthorizedExecution, ExecutionError> {
    authorize_with_verifier(
        spec_bytes,
        grant_bytes,
        policy,
        &MetadataOnlyEvidenceVerifier,
    )
}

/// Validate and bind execution values after the embedding authority has
/// verified every external identity/provenance evidence reference.
pub fn authorize_with_verifier(
    spec_bytes: &[u8],
    grant_bytes: &[u8],
    policy: &AuthorizationPolicy,
    verifier: &dyn EvidenceVerifier,
) -> Result<AuthorizedExecution, ExecutionError> {
    let spec_message = read_message(spec_bytes, "RunSpec")?;
    let grant_message = read_message(grant_bytes, "RunGrant")?;
    let spec = spec_message
        .get_root::<execution_capnp::run_spec::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid RunSpec root: {error}")))?;
    let grant = grant_message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid RunGrant root: {error}")))?;

    let schema_version = text(spec.get_schema_version(), "RunSpec.schemaVersion")?;
    if schema_version != EXECUTION_SCHEMA_VERSION {
        return Err(ExecutionError::invalid(format!(
            "unsupported execution schema version: {schema_version}"
        )));
    }

    let grant_id = nonempty(text(grant.get_grant_id(), "RunGrant.grantId")?, "grantId")?;
    let replay_key = nonempty(
        text(grant.get_replay_key(), "RunGrant.replayKey")?,
        "replayKey",
    )?;
    if grant.get_expires_at_unix_ms() <= policy.now_unix_ms {
        return Err(ExecutionError::invalid("RunGrant has expired"));
    }

    let spec_digest = canonical_digest(spec_bytes)?;
    let grant_digest = canonical_digest(grant_bytes)?;
    let bound_digest = read_digest(grant.get_run_spec_digest(), "RunGrant.runSpecDigest")?;
    if bound_digest != spec_digest {
        return Err(ExecutionError::invalid(
            "RunGrant.runSpecDigest does not bind the supplied RunSpec",
        ));
    }

    validate_evidence(grant.get_issuer_evidence(), "issuerEvidence", verifier)?;
    validate_evidence(
        grant.get_workload_identity_evidence(),
        "workloadIdentityEvidence",
        verifier,
    )?;
    validate_evidence(
        grant.get_actor_provenance_evidence(),
        "actorProvenanceEvidence",
        verifier,
    )?;
    let confinement_digest =
        read_digest(grant.get_confinement_digest(), "RunGrant.confinementDigest")?;
    if let Some(required) = &policy.required_confinement_digest
        && required != &confinement_digest
    {
        return Err(ExecutionError::identity_mismatch(
            "RunGrant confinement digest does not match the enforcement policy",
        ));
    }
    validate_limits(spec.get_requested_limits(), grant.get_limits())?;
    validate_workspaces(spec.get_workspace_inputs(), grant.get_workspaces())?;
    let intent = read_intent(&spec)?;

    let mut has_execution_capability = false;
    let capabilities = grant
        .get_capabilities()
        .map_err(|error| ExecutionError::invalid(format!("invalid capabilities: {error}")))?;
    for capability in capabilities {
        let name = text(capability.get_grant(), "capability.grant")?;
        let interface = text(capability.get_interface(), "capability.interface")?;
        if name == EXECUTION_CAPABILITY && interface == EXECUTION_SCHEMA_VERSION {
            has_execution_capability = true;
        }
    }
    if !has_execution_capability {
        return Err(ExecutionError::invalid(
            "RunGrant lacks the execution/v1 run capability",
        ));
    }

    let backend = match grant
        .get_backend_class()
        .map_err(|error| ExecutionError::invalid(format!("invalid backend class: {error}")))?
    {
        execution_capnp::BackendClass::Native => BackendClass::Native,
        execution_capnp::BackendClass::MicroVm => BackendClass::MicroVm,
    };
    if backend != policy.required_backend {
        return Err(ExecutionError::unsupported(format!(
            "grant requires {backend:?}, policy requires {:?}",
            policy.required_backend
        )));
    }

    // A run ID is derived from the bound authority and intent. This makes
    // retries stable while preventing callers from selecting arbitrary IDs.
    let mut run_id_material = Vec::new();
    run_id_material.extend_from_slice(spec_bytes);
    run_id_material.extend_from_slice(grant_id.as_bytes());
    run_id_material.extend_from_slice(replay_key.as_bytes());
    let run_id = format!("run-{}", run_id_material.hash());

    let allowed_egress = grant
        .get_allowed_egress()
        .map_err(|error| ExecutionError::invalid(format!("invalid allowedEgress: {error}")))?
        .iter()
        .map(|value| text(value, "allowedEgress entry"))
        .collect::<Result<Vec<_>, _>>()?;
    let credential_brokers = grant.get_credential_broker_refs().map_err(|error| {
        ExecutionError::invalid(format!("invalid credentialBrokerRefs: {error}"))
    })?;
    if !credential_brokers.is_empty() {
        return Err(ExecutionError::unsupported(
            "credential broker authority requires a broker integration",
        ));
    }

    Ok(AuthorizedExecution {
        run_id,
        grant_id,
        replay_key,
        spec_digest,
        grant_digest,
        confinement_digest,
        backend,
        allowed_egress,
        intent,
    })
}

fn read_message(
    bytes: &[u8],
    name: &str,
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>, ExecutionError> {
    let mut input = bytes;
    capnp::serialize::read_message(&mut input, ReaderOptions::new())
        .map_err(|error| ExecutionError::invalid(format!("invalid {name} message: {error}")))
}

fn text<'a>(
    value: capnp::Result<capnp::text::Reader<'a>>,
    field: &str,
) -> Result<String, ExecutionError> {
    value
        .map_err(|error| ExecutionError::invalid(format!("invalid {field}: {error}")))?
        .to_str()
        .map(str::to_owned)
        .map_err(|error| ExecutionError::invalid(format!("{field} is not UTF-8: {error}")))
}

fn nonempty(value: String, field: &str) -> Result<String, ExecutionError> {
    if value.is_empty() {
        Err(ExecutionError::invalid(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(value)
    }
}

/// Return the digest of a schema message's Cap'n Proto canonical form.
///
/// The transport framing (segment table, padding, and segmentation) is not
/// part of the content identity used by `RunGrant.runSpecDigest`.
pub fn canonical_digest(bytes: &[u8]) -> Result<String, ExecutionError> {
    let message = read_message(bytes, "RunSpec")?;
    let canonical = message.canonicalize().map_err(|error| {
        ExecutionError::invalid(format!("cannot canonicalize RunSpec: {error}"))
    })?;
    Ok(format!(
        "blake3-256:{}",
        capnp::Word::words_to_bytes(&canonical).hash()
    ))
}

fn read_digest(
    value: capnp::Result<execution_capnp::digest_ref::Reader<'_>>,
    field: &str,
) -> Result<String, ExecutionError> {
    let digest =
        value.map_err(|error| ExecutionError::invalid(format!("invalid {field}: {error}")))?;
    let algorithm = text(digest.get_algorithm(), &format!("{field}.algorithm"))?;
    let value = text(digest.get_value(), &format!("{field}.value"))?;
    if algorithm != "blake3-256"
        || value.len() != 64
        || !value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(ExecutionError::invalid(format!(
            "{field} must be a lowercase blake3-256 digest"
        )));
    }
    Ok(format!("{algorithm}:{value}"))
}

fn validate_evidence(
    value: capnp::Result<execution_capnp::evidence_ref::Reader<'_>>,
    field: &str,
    verifier: &dyn EvidenceVerifier,
) -> Result<(), ExecutionError> {
    let evidence =
        value.map_err(|error| ExecutionError::invalid(format!("invalid {field}: {error}")))?;
    let media_type = nonempty(
        text(evidence.get_media_type(), &format!("{field}.mediaType"))?,
        field,
    )?;
    let digest = read_digest(evidence.get_digest(), &format!("{field}.digest"))?;
    verifier.verify(field, &EvidenceRef { media_type, digest })?;
    Ok(())
}

fn read_intent(
    spec: &execution_capnp::run_spec::Reader<'_>,
) -> Result<SchemaIntent, ExecutionError> {
    let requested_interfaces = spec.get_requested_interfaces().map_err(|error| {
        ExecutionError::invalid(format!("invalid requestedInterfaces: {error}"))
    })?;
    for interface in requested_interfaces.iter() {
        let interface = text(interface, "requestedInterfaces entry")?;
        if interface != EXECUTION_SCHEMA_VERSION {
            return Err(ExecutionError::unsupported(format!(
                "requested interface is not supported by this LLO execution surface: {interface}"
            )));
        }
    }
    if !spec
        .get_secret_handles()
        .map_err(|error| ExecutionError::invalid(format!("invalid secretHandles: {error}")))?
        .is_empty()
    {
        return Err(ExecutionError::unsupported(
            "secret handles require a credential broker integration",
        ));
    }
    if !spec
        .get_outputs()
        .map_err(|error| ExecutionError::invalid(format!("invalid outputs: {error}")))?
        .is_empty()
    {
        return Err(ExecutionError::unsupported(
            "declared outputs require the collect/output backend",
        ));
    }
    if spec
        .get_cancellation_mode()
        .map_err(|error| ExecutionError::invalid(format!("invalid cancellationMode: {error}")))?
        != execution_capnp::CancellationMode::ExplicitOnly
    {
        return Err(ExecutionError::unsupported(
            "cancel-on-disconnect is not supported by this execution surface",
        ));
    }
    if spec.has_compatibility_runtime() {
        return Err(ExecutionError::unsupported(
            "compatibility runtime selection is not supported by this execution surface",
        ));
    }
    let executable = spec
        .get_executable()
        .map_err(|error| ExecutionError::invalid(format!("invalid executable: {error}")))?;
    let media_type = nonempty(
        text(executable.get_media_type(), "executable.mediaType")?,
        "executable.mediaType",
    )?;
    let arguments = spec
        .get_arguments()
        .map_err(|error| ExecutionError::invalid(format!("invalid arguments: {error}")))?
        .iter()
        .map(|value| text(value, "arguments entry"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut public_environment = BTreeMap::new();
    for entry in spec
        .get_public_environment()
        .map_err(|error| ExecutionError::invalid(format!("invalid publicEnvironment: {error}")))?
    {
        let key = nonempty(text(entry.get_key(), "environment key")?, "environment key")?;
        let value = text(entry.get_value(), "environment value")?;
        if public_environment.insert(key.clone(), value).is_some() {
            return Err(ExecutionError::invalid(format!(
                "duplicate public environment key: {key}"
            )));
        }
    }
    let workspace_inputs = spec
        .get_workspace_inputs()
        .map_err(|error| ExecutionError::invalid(format!("invalid workspaceInputs: {error}")))?
        .iter()
        .map(|workspace| {
            Ok(WorkspaceInput {
                name: nonempty(
                    text(workspace.get_name(), "workspaceInputs.name")?,
                    "workspace name",
                )?,
                graph_root: read_digest(workspace.get_graph_root(), "workspaceInputs.graphRoot")?,
            })
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
    let limits = spec
        .get_requested_limits()
        .map_err(|error| ExecutionError::invalid(format!("invalid requestedLimits: {error}")))?;

    Ok(SchemaIntent {
        executable: ArtifactIdentity {
            digest: read_digest(executable.get_digest(), "executable.digest")?,
            media_type,
        },
        arguments,
        public_environment,
        workspace_inputs,
        requested_limits: SchemaLimits {
            wall_time_ms: limits.get_wall_time_ms(),
            memory_bytes: limits.get_memory_bytes(),
            cpu_millis: limits.get_cpu_millis(),
            output_bytes: limits.get_output_bytes(),
        },
    })
}

fn validate_limits(
    requested: capnp::Result<execution_capnp::resource_limits::Reader<'_>>,
    resolved: capnp::Result<execution_capnp::resource_limits::Reader<'_>>,
) -> Result<(), ExecutionError> {
    let requested = requested
        .map_err(|error| ExecutionError::invalid(format!("invalid requestedLimits: {error}")))?;
    let resolved = resolved
        .map_err(|error| ExecutionError::invalid(format!("invalid grant limits: {error}")))?;
    for (name, requested, resolved) in [
        (
            "wallTimeMs",
            requested.get_wall_time_ms(),
            resolved.get_wall_time_ms(),
        ),
        (
            "memoryBytes",
            requested.get_memory_bytes(),
            resolved.get_memory_bytes(),
        ),
        (
            "cpuMillis",
            requested.get_cpu_millis(),
            resolved.get_cpu_millis(),
        ),
        (
            "outputBytes",
            requested.get_output_bytes(),
            resolved.get_output_bytes(),
        ),
    ] {
        if requested != 0 && (resolved == 0 || resolved > requested) {
            return Err(ExecutionError::invalid(format!(
                "grant limit {name} widens the requested limit"
            )));
        }
    }
    Ok(())
}

fn validate_workspaces(
    requested: capnp::Result<
        capnp::struct_list::Reader<'_, execution_capnp::workspace_intent::Owned>,
    >,
    resolved: capnp::Result<
        capnp::struct_list::Reader<'_, execution_capnp::workspace_grant::Owned>,
    >,
) -> Result<(), ExecutionError> {
    let requested = requested
        .map_err(|error| ExecutionError::invalid(format!("invalid workspaceInputs: {error}")))?;
    let resolved = resolved
        .map_err(|error| ExecutionError::invalid(format!("invalid grant workspaces: {error}")))?;
    let requested = requested
        .iter()
        .map(|workspace| {
            Ok((
                text(workspace.get_name(), "workspaceInputs.name")?,
                read_digest(workspace.get_graph_root(), "workspaceInputs.graphRoot")?,
            ))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
    let resolved = resolved
        .iter()
        .map(|workspace| {
            let name = text(workspace.get_name(), "grant workspace name")?;
            let graph_root = read_digest(workspace.get_graph_root(), "grant workspace graphRoot")?;
            let operations = workspace.get_operations().map_err(|error| {
                ExecutionError::invalid(format!("invalid operations for {name}: {error}"))
            })?;
            if operations.is_empty() {
                return Err(ExecutionError::invalid(format!(
                    "grant workspace {name} has no authorized operations"
                )));
            }
            Ok((name, graph_root))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;

    // A grant must resolve exactly the requested workspace set.  A strict
    // equality check prevents an omitted workspace from silently becoming an
    // empty/implicitly-authorized input, and duplicate names are ambiguous.
    if requested.len() != resolved.len()
        || requested.iter().any(|entry| !resolved.contains(entry))
        || resolved.iter().any(|entry| {
            resolved
                .iter()
                .filter(|candidate| candidate.0 == entry.0)
                .count()
                != 1
        })
    {
        return Err(ExecutionError::invalid(
            "grant workspaces must exactly match RunSpec.workspaceInputs",
        ));
    }
    Ok(())
}
