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
    /// Wall clock used to evaluate `RunGrant.expiresAtUnixMs`.
    ///
    /// `None` — the default — samples the clock on **every** authorization.
    /// A policy is built once and reused for the life of a daemon, so a
    /// captured timestamp would make every grant that was valid at startup
    /// valid forever; expiry would stop being enforced after the first
    /// millisecond of uptime. `Some` pins the clock, for tests and for
    /// embedders that supply their own trusted time source.
    pub now_unix_ms: Option<u64>,
    pub required_backend: BackendClass,
    /// Digest of the confinement policy the selected backend will enforce.
    /// A grant for any other policy is rejected before resolver invocation.
    pub required_confinement_digest: Option<String>,
}

/// Which authority a `RunGrant` evidence reference is claimed to carry.
///
/// The three references are structurally identical, so a role passed as a
/// `&str` is only as good as the caller's discipline — and the original
/// implementation used it for error strings alone, which is how one trusted
/// envelope came to satisfy all three at once. A closed enum makes the roles
/// distinguishable by type, and gives a verifier a value it can require the
/// evidence to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvidenceField {
    Issuer,
    WorkloadIdentity,
    ActorProvenance,
}

impl EvidenceField {
    /// The `RunGrant` field name — and the in-toto subject name a statement
    /// must carry for the statement to assert this role.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issuer => "issuerEvidence",
            Self::WorkloadIdentity => "workloadIdentityEvidence",
            Self::ActorProvenance => "actorProvenanceEvidence",
        }
    }
}

impl std::fmt::Display for EvidenceField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The run an evidence reference must authorize.
///
/// Evidence that merely *exists* and verifies is not authority to execute:
/// without this, any envelope in the trusted catalog authorizes any run. The
/// run identity is derived from the bound spec digest and the grant's
/// identity fields before any evidence is checked, so a verifier can require
/// the evidence to commit to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceBinding {
    /// Content-addressed identity of the run being authorized, in the
    /// `run-<64 hex>` form [`derive_run_id`] produces.
    pub run_id: String,
    /// Canonical digest of the `RunSpec` the grant binds.
    pub spec_digest: String,
}

/// A `RunGrant`'s detached issuer signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSignature {
    /// Signature algorithm; `ed25519` is the only value execution/v1 defines.
    pub algorithm: String,
    /// Unauthenticated issuer hint. A verifier must check every trusted key
    /// regardless and never select one by this — the same parity-not-lookup
    /// rule DSSE's `keyid` follows (signet ADR-012 R1).
    pub key_id: String,
    pub value: Vec<u8>,
}

/// A `RunGrant` presented for authorization, with the exact bytes its issuer
/// signature covers already computed.
///
/// A verifier is handed the covered bytes rather than the grant, so it cannot
/// disagree with the substrate about what was signed — the classic
/// reimplementation bug in detached-signature schemes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedGrant {
    /// Cap'n Proto canonical bytes of the grant with `signature` cleared.
    /// A valid signature is over
    /// `PAE(GRANT_SIGNATURE_PAYLOAD_TYPE, signing_bytes)`.
    pub signing_bytes: Vec<u8>,
    /// The carried signature; `None` when the grant is unsigned.
    pub signature: Option<GrantSignature>,
}

/// DSSE pre-authentication payload type separating a `RunGrant` signature
/// from every other Ed25519 signature in the substrate.
pub const GRANT_SIGNATURE_PAYLOAD_TYPE: &str = "application/vnd.cloister.execution.run-grant+capnp";

/// A trust-domain adapter supplied by the embedding authority (Cloister /
/// Interlace). LLO deliberately does not own Signet/NotMe trust roots.
///
/// One adapter owns the whole execution/v1 trust domain: the issuer signature
/// on the grant, and each of the three evidence references it carries.
///
/// `EvidenceRef` is a CAS reference, not proof by itself. An adapter must
/// resolve the referenced canonical bytes and verify the appropriate signed
/// envelope/certificate chain before authorization can be called with it —
/// and must check that what it verified authorizes `binding`, not merely
/// that it verifies.
pub trait EvidenceVerifier: Send + Sync {
    fn verify(
        &self,
        field: EvidenceField,
        binding: &EvidenceBinding,
        evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError>;

    /// Check that this grant's fields were authorized by an issuer this trust
    /// domain accepts. Every other check in `authorize` reads fields the
    /// grant chose for itself; without this one, nothing ties `capabilities`,
    /// `limits`, `backendClass`, `confinementDigest` or `expiresAtUnixMs` to
    /// an authority.
    fn verify_grant(&self, grant: &SignedGrant) -> Result<(), ExecutionError>;
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
    fn verify(
        &self,
        field: EvidenceField,
        binding: &EvidenceBinding,
        evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError> {
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
        // The statement must assert *this* role for *this* run. A subject
        // named for the role is the issuer's assertion of it; the subject's
        // BLAKE3 digest is the run it commits to. Without both, one trusted
        // envelope authorizes every run and every role.
        let mut asserts_role = false;
        for subject in statement.subject() {
            if subject.name() != field.as_str() {
                continue;
            }
            asserts_role = true;
            let Some(claimed) = subject.digest(RUN_BINDING_ALGORITHM) else {
                continue;
            };
            // ADR-012 R4: gate the identifier against its shape here rather
            // than comparing whatever string arrived, and R1: compare for
            // equality against a run identity we derived — never look
            // anything up by it.
            if !is_run_id(claimed) {
                return Err(ExecutionError {
                    code: crate::ErrorCode::Unauthenticated,
                    retryable: false,
                    detail: format!("{field} subject digest is not a run identity"),
                });
            }
            if claimed == binding.run_id {
                return Ok(());
            }
        }
        Err(ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail: if asserts_role {
                format!("{field} does not authorize this run")
            } else {
                format!("{field} statement asserts no {field} subject")
            },
        })
    }

    fn verify_grant(&self, grant: &SignedGrant) -> Result<(), ExecutionError> {
        let unauthenticated = |detail: String| ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail,
        };
        let signature = grant
            .signature
            .as_ref()
            .ok_or_else(|| unauthenticated("RunGrant carries no issuer signature".into()))?;
        if signature.algorithm != GRANT_SIGNATURE_ALGORITHM {
            return Err(unauthenticated(format!(
                "RunGrant signature algorithm {:?} is not supported",
                signature.algorithm
            )));
        }
        let value: [u8; 64] = signature.value.as_slice().try_into().map_err(|_| {
            unauthenticated(format!(
                "RunGrant ed25519 signature must be 64 bytes, got {}",
                signature.value.len()
            ))
        })?;
        // Parity, not lookup: every trusted key is tried and `key_id` selects
        // nothing, so an attacker-chosen hint cannot steer verification.
        let trusted = self.trust_keys.iter().any(|key| {
            leyline_envelope::verify_payload(
                GRANT_SIGNATURE_PAYLOAD_TYPE,
                &grant.signing_bytes,
                &value,
                key,
            )
        });
        if trusted {
            Ok(())
        } else {
            Err(unauthenticated(
                "RunGrant signature is not by a trusted issuer".into(),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub media_type: String,
    pub digest: String,
}

/// Compatibility verifier for explicitly opted-in fixture tests only.
/// Production integrations must use an embedding-owned verifier and call
/// [`authorize_with_verifier`]. It intentionally performs no authentication.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetadataOnlyEvidenceVerifier;

impl EvidenceVerifier for MetadataOnlyEvidenceVerifier {
    fn verify(
        &self,
        _field: EvidenceField,
        _binding: &EvidenceBinding,
        _evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError> {
        Ok(())
    }

    fn verify_grant(&self, _grant: &SignedGrant) -> Result<(), ExecutionError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RejectUnverifiedEvidence;

impl EvidenceVerifier for RejectUnverifiedEvidence {
    fn verify(
        &self,
        field: EvidenceField,
        _binding: &EvidenceBinding,
        _evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError> {
        Err(ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail: format!("no trusted verifier configured for {field}"),
        })
    }

    fn verify_grant(&self, _grant: &SignedGrant) -> Result<(), ExecutionError> {
        Err(ExecutionError {
            code: crate::ErrorCode::Unauthenticated,
            retryable: false,
            detail: "no trusted verifier configured for RunGrant.signature".into(),
        })
    }
}

impl Default for AuthorizationPolicy {
    fn default() -> Self {
        Self {
            now_unix_ms: None,
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
    /// The confinement/v1 document `confinement_digest` was taken over, when
    /// the issuer carried it.
    ///
    /// Verified self-consistent at authorization: if present, its digest MUST
    /// equal `confinement_digest`, so a grant cannot name one policy and carry
    /// another. `None` means the issuer committed by digest alone — a runner
    /// that cannot otherwise obtain that document knows only whether what it
    /// applied matched, never what was authorized.
    pub confinement_manifest: Option<String>,
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
    authorize_with_verifier(spec_bytes, grant_bytes, policy, &RejectUnverifiedEvidence)
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

    // Before any field of the grant is read as authority, establish that an
    // issuer authorized those fields. Everything below this line is the
    // grant describing itself.
    verifier.verify_grant(&SignedGrant {
        signing_bytes: grant_signing_bytes(grant_bytes)?,
        signature: read_grant_signature(&grant)?,
    })?;

    let grant_id = nonempty(text(grant.get_grant_id(), "RunGrant.grantId")?, "grantId")?;
    let replay_key = nonempty(
        text(grant.get_replay_key(), "RunGrant.replayKey")?,
        "replayKey",
    )?;
    let now_unix_ms = policy.now_unix_ms.unwrap_or_else(current_unix_ms);
    if grant.get_expires_at_unix_ms() <= now_unix_ms {
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

    // A run ID is derived from the bound authority and intent. This makes
    // retries stable, prevents callers from selecting arbitrary IDs, and lets
    // a second implementation derive the same name from the same content.
    // It is derived *here*, before any evidence is checked, because evidence
    // must commit to the run identity — so the identity has to exist first.
    let run_id = derive_run_id(&spec_digest, &grant_id, &replay_key);
    let binding = EvidenceBinding {
        run_id: run_id.clone(),
        spec_digest: spec_digest.clone(),
    };

    validate_evidence(
        grant.get_issuer_evidence(),
        EvidenceField::Issuer,
        &binding,
        verifier,
    )?;
    validate_evidence(
        grant.get_workload_identity_evidence(),
        EvidenceField::WorkloadIdentity,
        &binding,
        verifier,
    )?;
    validate_evidence(
        grant.get_actor_provenance_evidence(),
        EvidenceField::ActorProvenance,
        &binding,
        verifier,
    )?;
    let confinement_digest =
        read_digest(grant.get_confinement_digest(), "RunGrant.confinementDigest")?;

    // If the issuer carried the document, it must be the document the digest
    // was taken over. Without this the field would be decoration: a grant could
    // name policy A by digest and carry policy B, and a runner reading the
    // carried one would enforce something the issuer never authorized while the
    // digest check still passed.
    //
    // Parsed, not merely hashed. `ConfinementManifest::parse` applies every §2-§5
    // refusal, so a document that hashes correctly but violates the spec is
    // refused here rather than at the backend — and a grant cannot smuggle a
    // schema-invalid policy past authorization by having the right digest.
    let confinement_manifest = {
        let carried = text(
            grant.get_confinement_manifest(),
            "RunGrant.confinementManifest",
        )?;
        if carried.is_empty() {
            None
        } else {
            let parsed =
                crate::confinement::ConfinementManifest::parse(&carried).map_err(|error| {
                    ExecutionError::invalid(format!(
                        "RunGrant.confinementManifest is not a valid confinement/v1 document: {}",
                        error.detail
                    ))
                })?;
            let carried_digest = parsed.confinement_digest()?;
            if carried_digest != confinement_digest {
                return Err(ExecutionError::identity_mismatch(format!(
                    "RunGrant carries a confinement manifest digesting to                      {carried_digest}, but confinementDigest names                      {confinement_digest}"
                )));
            }
            Some(carried)
        }
    };
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
        confinement_manifest,
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

/// The only signature algorithm execution/v1 defines for a `RunGrant`.
const GRANT_SIGNATURE_ALGORITHM: &str = "ed25519";

/// The exact bytes a `RunGrant`'s issuer signature covers: the grant's
/// Cap'n Proto **canonical** form with the `signature` field cleared.
///
/// Two properties make this the signable form. Canonical, so the signature
/// survives re-framing — segment layout and padding are an encoder's choice,
/// not content, exactly as for `runSpecDigest`. Signature-cleared, so signing
/// and verifying agree without a second encoding: an issuer computes these
/// bytes from the grant it is about to sign, and a verifier recomputes them
/// from the grant it received, signature and all.
///
/// A cleared `signature` canonicalizes to a null pointer in the trailing
/// pointer slot, which canonicalization truncates — so these bytes are also
/// what a producer that predates the field would have emitted.
pub fn grant_signing_bytes(grant_bytes: &[u8]) -> Result<Vec<u8>, ExecutionError> {
    let message = read_message(grant_bytes, "RunGrant")?;
    let grant = message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid RunGrant root: {error}")))?;
    let mut builder = capnp::message::Builder::new_default();
    builder
        .set_root(grant)
        .map_err(|error| ExecutionError::invalid(format!("cannot copy RunGrant: {error}")))?;
    builder
        .get_root::<execution_capnp::run_grant::Builder<'_>>()
        .map_err(|error| ExecutionError::invalid(format!("invalid RunGrant root: {error}")))?
        .init_signature();
    let canonical = builder.into_reader().canonicalize().map_err(|error| {
        ExecutionError::invalid(format!("cannot canonicalize RunGrant: {error}"))
    })?;
    Ok(capnp::Word::words_to_bytes(&canonical).to_vec())
}

fn read_grant_signature(
    grant: &execution_capnp::run_grant::Reader<'_>,
) -> Result<Option<GrantSignature>, ExecutionError> {
    if !grant.has_signature() {
        return Ok(None);
    }
    let signature = grant
        .get_signature()
        .map_err(|error| ExecutionError::invalid(format!("invalid RunGrant.signature: {error}")))?;
    Ok(Some(GrantSignature {
        algorithm: text(signature.get_algorithm(), "RunGrant.signature.algorithm")?,
        key_id: text(signature.get_key_id(), "RunGrant.signature.keyId")?,
        value: signature
            .get_value()
            .map_err(|error| {
                ExecutionError::invalid(format!("invalid RunGrant.signature.value: {error}"))
            })?
            .to_vec(),
    }))
}

/// Domain separator for run-identity derivation.
const RUN_ID_DOMAIN: &[u8] = b"cloister/execution/v1/run-id";

/// Prefix carried by every derived run identity.
const RUN_ID_PREFIX: &str = "run-";

/// Digest algorithm under which an in-toto subject binds evidence to a run.
///
/// BLAKE3, because a run identity is a Σ content address. Per signet ADR-012
/// SHA-256 names a *key* (kid, JWKS, X.509 SKI, DPoP jkt) and BLAKE3
/// addresses content; digesting a run identity under `sha256` would be that
/// category error in reverse. The in-toto digest set is open, so this
/// coexists with the `sha256` subject digests the same producer writes for
/// file artifacts.
const RUN_BINDING_ALGORITHM: &str = "blake3";

/// Whether `value` has the shape [`derive_run_id`] produces.
///
/// ADR-012 R4: an identifier crossing a trust boundary is gated against its
/// shape before it is used, so a malformed value fails as malformed rather
/// than as a mismatch — and so the gate stays a gate if a later caller is
/// tempted to use the value for anything but equality.
fn is_run_id(value: &str) -> bool {
    value.strip_prefix(RUN_ID_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Milliseconds since the Unix epoch, sampled when a policy pins no clock.
pub fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Derive the content-addressed name of a run from its authorized identity.
///
/// The inputs are the spec's **canonical** digest — not the received wire
/// bytes, whose segment layout is an encoder choice rather than content — and
/// the grant's identity fields, each length-prefixed. A bare concatenation is
/// ambiguous at the field boundaries: `("ab", "c")` and `("a", "bc")` would
/// otherwise share one run identity. Both properties are required for a
/// consumer to compute this id locally and address a run without waiting for
/// `start` to return; `execution/v1/test-vectors/run-id.json` pins the
/// derivation for cross-implementation conformance.
pub fn derive_run_id(canonical_spec_digest: &str, grant_id: &str, replay_key: &str) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(RUN_ID_DOMAIN);
    material.push(0);
    for field in [
        canonical_spec_digest.as_bytes(),
        grant_id.as_bytes(),
        replay_key.as_bytes(),
    ] {
        material.extend_from_slice(&(field.len() as u64).to_le_bytes());
        material.extend_from_slice(field);
    }
    format!("{RUN_ID_PREFIX}{}", material.hash())
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
    field: EvidenceField,
    binding: &EvidenceBinding,
    verifier: &dyn EvidenceVerifier,
) -> Result<(), ExecutionError> {
    let evidence =
        value.map_err(|error| ExecutionError::invalid(format!("invalid {field}: {error}")))?;
    let media_type = nonempty(
        text(evidence.get_media_type(), &format!("{field}.mediaType"))?,
        field.as_str(),
    )?;
    let digest = read_digest(evidence.get_digest(), &format!("{field}.digest"))?;
    verifier.verify(field, binding, &EvidenceRef { media_type, digest })?;
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
