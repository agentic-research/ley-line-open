//! Schema-backed authorization for execution/v1.
//!
//! `RunSpec` is caller-controlled intent.  `RunGrant` is the only object that
//! can turn that intent into an accepted run.  This module deliberately reads
//! the generated Cap'n Proto surface from `leyline-public-schema`; it does not
//! define a second wire model.  Backend/rootfs resolution happens after this
//! check, through a trusted resolver owned by the runtime.

use std::time::{SystemTime, UNIX_EPOCH};

use capnp::message::ReaderOptions;
use leyline_public_schema::execution_capnp;

use crate::{BackendClass, ExecutionError};

pub const EXECUTION_SCHEMA_VERSION: &str = "cloister/execution/v1";
pub const EXECUTION_CAPABILITY: &str = "urn:signet:cap:execute:run";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationPolicy {
    pub now_unix_ms: u64,
    pub required_backend: BackendClass,
}

impl Default for AuthorizationPolicy {
    fn default() -> Self {
        Self {
            now_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            required_backend: BackendClass::MicroVm,
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
    pub confinement_digest: String,
    pub backend: BackendClass,
    pub allowed_egress: Vec<String>,
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
    let bound_digest = read_digest(grant.get_run_spec_digest(), "RunGrant.runSpecDigest")?;
    if bound_digest != spec_digest {
        return Err(ExecutionError::invalid(
            "RunGrant.runSpecDigest does not bind the supplied RunSpec",
        ));
    }

    validate_evidence(grant.get_issuer_evidence(), "issuerEvidence")?;
    validate_evidence(
        grant.get_workload_identity_evidence(),
        "workloadIdentityEvidence",
    )?;
    validate_evidence(
        grant.get_actor_provenance_evidence(),
        "actorProvenanceEvidence",
    )?;
    let confinement_digest =
        read_digest(grant.get_confinement_digest(), "RunGrant.confinementDigest")?;

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
    let run_id = format!("run-{}", blake3::hash(&run_id_material).to_hex());

    let allowed_egress = grant
        .get_allowed_egress()
        .map_err(|error| ExecutionError::invalid(format!("invalid allowedEgress: {error}")))?
        .iter()
        .map(|value| text(value, "allowedEgress entry"))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AuthorizedExecution {
        run_id,
        grant_id,
        replay_key,
        spec_digest,
        confinement_digest,
        backend,
        allowed_egress,
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
        blake3::hash(capnp::Word::words_to_bytes(&canonical)).to_hex()
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
) -> Result<(), ExecutionError> {
    let evidence =
        value.map_err(|error| ExecutionError::invalid(format!("invalid {field}: {error}")))?;
    nonempty(
        text(evidence.get_media_type(), &format!("{field}.mediaType"))?,
        field,
    )?;
    let _ = read_digest(evidence.get_digest(), &format!("{field}.digest"))?;
    Ok(())
}
