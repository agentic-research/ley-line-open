//! Ed25519 certificate verification and claims extraction for certificates
//! that carry extensions under the `1.3.6.1.4.1.99999.1.*` arc.
//!
//! Consumers: cloister's lease middleware (cloister-bd7770) calls this
//! through the wasm32 build (`leyline_verify_cert_chain`,
//! `leyline_verify_cert_path`) to check that an ephemeral certificate was
//! issued under a pinned Ed25519 trust anchor and to read its claims.
//!
//! # Two profiles share one arc
//!
//! `1.3.6.1.4.1.99999` is the IANA example / private-experiment arc. Two
//! independent allocations live under it and assign different meanings to
//! the same OIDs (bead `ley-line-open-9ac66d`):
//!
//! | OID      | Interlace profile (cloister spec, LLO) | Signet profile (notme, signet)                |
//! |----------|----------------------------------------|-----------------------------------------------|
//! | `.1.1`   | not allocated                          | subject                                       |
//! | `.1.2`   | not allocated                          | issuance time                                 |
//! | `.1.3`   | not allocated                          | scopes / capabilities                         |
//! | `.1.4`   | epoch, DER INTEGER                     | epoch, DER INTEGER (notme)                    |
//! | `.1.5`   | interlace-peer, DER UTF8String         | authMethod (notme) / agent name (signet)      |
//! | `.1.6`   | interlace-scope, DER UTF8String        | peer binding, 32 raw bytes (notme) / scope (signet) |
//! | `.1.7`   | confinementDigest, OCTET STRING (32)   | not allocated (notme defers it to LLO)        |
//! | `.1.8`   | not allocated                          | principal kind (notme)                        |
//! | `.1.9`   | not allocated                          | task scope (notme)                            |
//!
//! `.1.5` is the dangerous row: notme writes a DER UTF8String there, so a
//! verifier that reads `.1.5` as interlace-peer parses notme's authMethod
//! (`"passkey"`) as a peer fingerprint without any error.
//!
//! # The discriminator
//!
//! Before any arc extension is interpreted, the certificate is classified
//! from its own signed extension list:
//!
//! - **Signet profile** iff it carries at least one of `.1.1`, `.1.2`,
//!   `.1.3`, `.1.8`, `.1.9`. The Interlace allocation never uses these
//!   OIDs; every notme mint path (`mintGHABridgeCert`, `mintBridgeCertPair`,
//!   `mintIssuingCaCert`, `mintTaskCertPair`) and the signet Go authority
//!   write `.1.1` and `.1.2` unconditionally.
//! - **Interlace profile** otherwise.
//!
//! The rule is a property of the signed bytes, not of which key signed them:
//! cloister's production trust anchor is notme's master key, so the issuer
//! key alone cannot tell the profiles apart. It is not "try to parse and
//! ignore failures": `.1.5`/`.1.6` are read only under the Interlace profile,
//! and under that profile a malformed value rejects the certificate.
//!
//! Under the Signet profile `.1.5` and `.1.6` are **never reported**. Their
//! meaning differs inside the Signet family itself (notme: authMethod and a
//! 32-byte peer binding; signet Go: agent name and scope, raw UTF-8), and no
//! byte-level rule separates notme from signet, so any claim read from them
//! would be a guess. They are treated as unrecognized: ignored when
//! non-critical, rejected when critical. `.1.4` (epoch) and `.1.7`
//! (confinementDigest) mean the same thing in both profiles and are read in
//! both. The profile is reported in [`CertClaims::profile`].
//!
//! Residual risk, stated plainly: a Signet-family issuer that omitted all
//! five marker OIDs would be classified as Interlace and its `.1.5` read as
//! a peer fingerprint. No current issuer does this; the permanent fix is a
//! private enterprise number per project (notme-229dc3), which removes the
//! shared arc.
//!
//! # Standard extensions
//!
//! notme certificates carry critical BasicConstraints, KeyUsage and
//! SubjectAltName. These are recognized and processed:
//!
//! - BasicConstraints: an end-entity certificate must not be a CA; an
//!   intermediate must be a CA and its pathLenConstraint must admit the
//!   intermediates below it.
//! - KeyUsage: an end-entity certificate, when KeyUsage is present, must
//!   permit digitalSignature; an intermediate must carry KeyUsage with
//!   keyCertSign.
//! - SubjectAltName: must decode as GeneralNames. Its contents are not
//!   reported.
//!
//! Any other critical extension rejects the certificate (RFC 5280 §4.2).
//! Duplicate extensions reject the certificate (RFC 5280 §4.2).
//!
//! # Certificate format expectations
//!
//! - X.509 v3, Ed25519 signature (outer and `tbsCertificate.signature`),
//!   Ed25519 SubjectPublicKeyInfo (32 bytes).
//! - Validity as UTCTime or GeneralizedTime; returned, not enforced.
//!
//! Missing arc extensions are not an error: the extension claims are
//! `Option`, and the caller decides which ones it requires.

use const_oid::ObjectIdentifier;
use const_oid::db::rfc5280::{ID_CE_BASIC_CONSTRAINTS, ID_CE_KEY_USAGE, ID_CE_SUBJECT_ALT_NAME};
use der::{Decode, Encode, Reader};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use x509_cert::Certificate;
use x509_cert::ext::pkix::{BasicConstraints, KeyUsage, SubjectAltName};
use x509_cert::name::Name;

use crate::oid::ID_ED25519;

/// Upper bound on intermediates accepted by [`verify_cert_path`]. notme's
/// deepest path today is one Issuing CA tier; the cap bounds work done on
/// attacker-supplied input.
pub const MAX_INTERMEDIATES: usize = 4;

/// Which allocation of the `1.3.6.1.4.1.99999.1.*` arc a certificate was
/// read under. See the module documentation for the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertProfile {
    /// cloister Interlace profile: `.1.5` peer_fp and `.1.6` scope are read.
    Interlace,
    /// notme / signet profile: `.1.5` and `.1.6` are never reported.
    Signet,
}

impl CertProfile {
    /// Stable lowercase name used in the claims JSON (`"pr"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CertProfile::Interlace => "interlace",
            CertProfile::Signet => "signet",
        }
    }
}

/// Result of a successful verification + claims extraction.
#[derive(Debug, Clone)]
pub struct CertClaims {
    /// Ephemeral pubkey from the end-entity SubjectPublicKeyInfo.
    pub ephemeral_pubkey: [u8; 32],
    /// Validity window, Unix seconds. For a path, the intersection of every
    /// certificate's window (latest notBefore, earliest notAfter).
    pub not_before: i64,
    pub not_after: i64,
    /// Arc allocation the certificate was read under.
    pub profile: CertProfile,
    /// `.1.4` epoch. For a path, every certificate must carry the same epoch
    /// or none may carry one.
    pub epoch: Option<u32>,
    /// `.1.5` interlace-peer. Always `None` under [`CertProfile::Signet`].
    pub peer_fp: Option<String>,
    /// `.1.6` interlace-scope. Always `None` under [`CertProfile::Signet`].
    pub scope: Option<String>,
    /// Bead `ley-line-open-c79ea8`: BLAKE3-256 of the §6-canonical
    /// `ConfinementManifest` (cloister/confinement/v1). When `Some`, a
    /// substrate runner MUST verify at bundle-start that the manifest it is
    /// about to enforce hashes to this digest.
    pub confinement_digest: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("cert DER parse failed: {0}")]
    BadDer(String),
    #[error("cert signature algorithm is not Ed25519")]
    NotEd25519,
    #[error("cert SPKI is not Ed25519 or has wrong length")]
    BadSpki,
    #[error("master pubkey is not 32 bytes")]
    BadMasterKey,
    #[error("Ed25519 signature verification failed")]
    BadSignature,
    #[error("cert encoding failed (re-encoding tbs_certificate): {0}")]
    EncodingFailed(String),
    #[error("interlace extension malformed: {0}")]
    BadExtension(&'static str),
    #[error("cert has critical unknown extension OID {0}")]
    UnknownCriticalExtension(String),
    /// A well-formed certificate or path violates a structural rule:
    /// BasicConstraints, KeyUsage, pathLenConstraint, issuer/subject name
    /// chaining, or epoch / profile agreement across the path.
    #[error("certificate constraint violated: {0}")]
    ConstraintViolation(&'static str),
}

/// Interlace-profile OIDs (cloister interlace-spec/0.1.0/wire/cert-extensions.md).
mod oid_interlace {
    use const_oid::ObjectIdentifier;
    /// epoch — DER INTEGER. Same meaning in both profiles.
    pub const EPOCH: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.4");
    /// interlace-peer — DER UTF8String. Interlace profile only.
    pub const PEER_FP: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.5");
    /// interlace-scope — DER UTF8String. Interlace profile only.
    pub const SCOPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.6");
    /// confinementDigest — DER OctetString of exactly 32 bytes. Bead
    /// `ley-line-open-c79ea8`. notme records `.1.7` as LLO's.
    pub const CONFINEMENT_DIGEST: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.7");
}

/// Signet-profile marker OIDs (notme worker/src/cert-authority.ts, signet
/// pkg/sigid/identity.go). Presence of any one selects the Signet profile.
pub mod oid_signet {
    use const_oid::ObjectIdentifier;
    /// subject identity.
    pub const SUBJECT: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.1");
    /// issuance time.
    pub const ISSUANCE_TIME: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.2");
    /// scopes (notme) / capabilities (signet).
    pub const SCOPES: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.3");
    /// principal kind (notme).
    pub const PRINCIPAL_KIND: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.8");
    /// task scope (notme).
    pub const TASK_SCOPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.9");
    /// The marker set. The Interlace allocation uses none of these.
    pub const MARKERS: [ObjectIdentifier; 5] =
        [SUBJECT, ISSUANCE_TIME, SCOPES, PRINCIPAL_KIND, TASK_SCOPE];
}

/// Classify a certificate's extension list. See the module documentation.
pub fn classify_profile(cert: &Certificate) -> CertProfile {
    let marked = cert
        .tbs_certificate
        .extensions
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|e| oid_signet::MARKERS.contains(&e.extn_id));
    if marked {
        CertProfile::Signet
    } else {
        CertProfile::Interlace
    }
}

/// Position of a certificate in the path being verified.
#[derive(Debug, Clone, Copy)]
enum Role {
    /// The certificate whose key the caller will use (lease signer).
    EndEntity,
    /// A CA certificate; `below` intermediates sit between it and the leaf.
    Intermediate { below: usize },
}

/// Claims read from one certificate after its signature verified.
struct LinkClaims {
    pubkey: [u8; 32],
    not_before: i64,
    not_after: i64,
    profile: CertProfile,
    epoch: Option<u32>,
    peer_fp: Option<String>,
    scope: Option<String>,
    confinement_digest: Option<[u8; 32]>,
}

/// Verify that `cert_der` is an end-entity certificate signed by
/// `master_pubkey` and extract its claims. Equivalent to
/// [`verify_cert_path`] with no intermediates.
///
/// On failure, callers should report an opaque "auth failed" externally;
/// the variant is for logs and tests.
pub fn verify_cert_chain(cert_der: &[u8], master_pubkey: &[u8]) -> Result<CertClaims, ChainError> {
    verify_cert_path(cert_der, &[], master_pubkey)
}

/// Verify `leaf_der` through `intermediates_der` up to the Ed25519 trust
/// anchor `anchor_pubkey`, and return the leaf's claims.
///
/// `intermediates_der[0]` is the leaf's issuer, `intermediates_der[1]` that
/// certificate's issuer, and so on; the last one is signed by the anchor.
/// The anchor is a bare public key, so the topmost certificate's issuer name
/// is not checked against anything.
///
/// Enforced: Ed25519 signatures on every link, issuer name == signer subject
/// name (DER equality) below the anchor, the BasicConstraints / KeyUsage /
/// pathLenConstraint rules in the module documentation, one profile across
/// the path, and one epoch across the path (all equal, or all absent).
pub fn verify_cert_path(
    leaf_der: &[u8],
    intermediates_der: &[&[u8]],
    anchor_pubkey: &[u8],
) -> Result<CertClaims, ChainError> {
    let anchor: [u8; 32] = anchor_pubkey
        .try_into()
        .map_err(|_| ChainError::BadMasterKey)?;
    let anchor = VerifyingKey::from_bytes(&anchor).map_err(|_| ChainError::BadMasterKey)?;
    if intermediates_der.len() > MAX_INTERMEDIATES {
        return Err(ChainError::ConstraintViolation(
            "path has more intermediates than MAX_INTERMEDIATES",
        ));
    }

    let leaf = parse_cert(leaf_der)?;
    let intermediates = intermediates_der
        .iter()
        .map(|d| parse_cert(d))
        .collect::<Result<Vec<_>, _>>()?;

    // Walk from the anchor down to the leaf.
    let mut signer_key = anchor;
    let mut signer_subject: Option<&Name> = None;
    let mut links: Vec<LinkClaims> = Vec::with_capacity(intermediates.len() + 1);
    for (below, cert) in intermediates.iter().enumerate().rev() {
        let link = verify_link(
            cert,
            &signer_key,
            signer_subject,
            Role::Intermediate { below },
        )?;
        signer_key = VerifyingKey::from_bytes(&link.pubkey).map_err(|_| ChainError::BadSpki)?;
        signer_subject = Some(&cert.tbs_certificate.subject);
        links.push(link);
    }
    let leaf_link = verify_link(&leaf, &signer_key, signer_subject, Role::EndEntity)?;

    let mut not_before = leaf_link.not_before;
    let mut not_after = leaf_link.not_after;
    for link in &links {
        if link.profile != leaf_link.profile {
            return Err(ChainError::ConstraintViolation(
                "certificates in the path were read under different profiles",
            ));
        }
        if link.epoch != leaf_link.epoch {
            return Err(ChainError::ConstraintViolation(
                "epoch differs across the path",
            ));
        }
        not_before = not_before.max(link.not_before);
        not_after = not_after.min(link.not_after);
    }

    Ok(CertClaims {
        ephemeral_pubkey: leaf_link.pubkey,
        not_before,
        not_after,
        profile: leaf_link.profile,
        epoch: leaf_link.epoch,
        peer_fp: leaf_link.peer_fp,
        scope: leaf_link.scope,
        confinement_digest: leaf_link.confinement_digest,
    })
}

/// Split a buffer holding zero or more concatenated DER certificates (the
/// FFI encoding of `intermediates_der`). Each element must be one complete
/// TLV; trailing garbage rejects. At most [`MAX_INTERMEDIATES`] elements.
pub fn split_concatenated_der(buf: &[u8]) -> Result<Vec<&[u8]>, ChainError> {
    let mut out = Vec::new();
    if buf.is_empty() {
        return Ok(out);
    }
    let mut reader = der::SliceReader::new(buf).map_err(|e| ChainError::BadDer(e.to_string()))?;
    while !reader.is_finished() {
        if out.len() == MAX_INTERMEDIATES {
            return Err(ChainError::ConstraintViolation(
                "path has more intermediates than MAX_INTERMEDIATES",
            ));
        }
        let tlv = reader
            .tlv_bytes()
            .map_err(|e| ChainError::BadDer(e.to_string()))?;
        out.push(tlv);
    }
    Ok(out)
}

fn parse_cert(der_bytes: &[u8]) -> Result<Certificate, ChainError> {
    Certificate::from_der(der_bytes).map_err(|e| ChainError::BadDer(e.to_string()))
}

/// Verify one certificate's signature under `signer`, then read its claims
/// under `role`.
fn verify_link(
    cert: &Certificate,
    signer: &VerifyingKey,
    signer_subject: Option<&Name>,
    role: Role,
) -> Result<LinkClaims, ChainError> {
    if cert.signature_algorithm.oid != ID_ED25519
        || cert.tbs_certificate.signature.oid != ID_ED25519
    {
        return Err(ChainError::NotEd25519);
    }

    // The signature covers the DER encoding of tbs_certificate. DER is
    // canonical, so re-encoding the parsed structure reproduces the signed
    // bytes for any DER input; non-DER input fails signature verification.
    let tbs_der = cert
        .tbs_certificate
        .to_der()
        .map_err(|e| ChainError::EncodingFailed(e.to_string()))?;
    let sig_arr: [u8; 64] = cert
        .signature
        .raw_bytes()
        .try_into()
        .map_err(|_| ChainError::BadSignature)?;
    signer
        .verify(&tbs_der, &Signature::from_bytes(&sig_arr))
        .map_err(|_| ChainError::BadSignature)?;

    if let Some(expected) = signer_subject
        && cert.tbs_certificate.issuer != *expected
    {
        return Err(ChainError::ConstraintViolation(
            "issuer name does not match the signing certificate's subject",
        ));
    }

    let spki = &cert.tbs_certificate.subject_public_key_info;
    if spki.algorithm.oid != ID_ED25519 {
        return Err(ChainError::BadSpki);
    }
    let pubkey: [u8; 32] = spki
        .subject_public_key
        .raw_bytes()
        .try_into()
        .map_err(|_| ChainError::BadSpki)?;

    let not_before = time_to_unix(&cert.tbs_certificate.validity.not_before);
    let not_after = time_to_unix(&cert.tbs_certificate.validity.not_after);

    let ext = extract_extensions(cert, role)?;
    Ok(LinkClaims {
        pubkey,
        not_before,
        not_after,
        profile: ext.profile,
        epoch: ext.epoch,
        peer_fp: ext.peer_fp,
        scope: ext.scope,
        confinement_digest: ext.confinement_digest,
    })
}

/// Convert an x509-cert `Time` to Unix seconds.
fn time_to_unix(t: &x509_cert::time::Time) -> i64 {
    use x509_cert::time::Time;
    match t {
        Time::UtcTime(ut) => ut.to_unix_duration().as_secs() as i64,
        Time::GeneralTime(gt) => gt.to_unix_duration().as_secs() as i64,
    }
}

struct Extracted {
    profile: CertProfile,
    epoch: Option<u32>,
    peer_fp: Option<String>,
    scope: Option<String>,
    confinement_digest: Option<[u8; 32]>,
}

/// Classify the certificate, then read each extension under that profile.
fn extract_extensions(cert: &Certificate, role: Role) -> Result<Extracted, ChainError> {
    let exts = cert.tbs_certificate.extensions.as_deref().unwrap_or(&[]);
    for (i, e) in exts.iter().enumerate() {
        if exts[..i].iter().any(|prev| prev.extn_id == e.extn_id) {
            return Err(ChainError::BadExtension("duplicate extension"));
        }
    }

    let profile = classify_profile(cert);
    let interlace = profile == CertProfile::Interlace;

    let mut out = Extracted {
        profile,
        epoch: None,
        peer_fp: None,
        scope: None,
        confinement_digest: None,
    };
    let mut basic: Option<BasicConstraints> = None;
    let mut key_usage: Option<KeyUsage> = None;

    for ext in exts {
        let value = ext.extn_value.as_bytes();
        let id: ObjectIdentifier = ext.extn_id;
        if id == oid_interlace::EPOCH {
            out.epoch = Some(parse_epoch(value)?);
        } else if id == oid_interlace::CONFINEMENT_DIGEST {
            out.confinement_digest = Some(parse_confinement_digest(value)?);
        } else if interlace && id == oid_interlace::PEER_FP {
            out.peer_fp = Some(parse_utf8(value, "peer_fp is not DER UTF8String")?);
        } else if interlace && id == oid_interlace::SCOPE {
            out.scope = Some(parse_utf8(value, "scope is not DER UTF8String")?);
        } else if id == ID_CE_BASIC_CONSTRAINTS {
            basic = Some(
                BasicConstraints::from_der(value)
                    .map_err(|_| ChainError::BadExtension("BasicConstraints is malformed"))?,
            );
        } else if id == ID_CE_KEY_USAGE {
            key_usage = Some(
                KeyUsage::from_der(value)
                    .map_err(|_| ChainError::BadExtension("KeyUsage is malformed"))?,
            );
        } else if id == ID_CE_SUBJECT_ALT_NAME {
            SubjectAltName::from_der(value)
                .map_err(|_| ChainError::BadExtension("SubjectAltName is malformed"))?;
        } else if ext.critical {
            // RFC 5280 §4.2: reject a critical extension this verifier does
            // not process. Under the Signet profile this includes `.1.5` and
            // `.1.6`, which are deliberately not read.
            return Err(ChainError::UnknownCriticalExtension(id.to_string()));
        }
        // Non-critical unrecognized extensions are ignored per RFC 5280.
    }

    match role {
        Role::EndEntity => {
            if basic.as_ref().is_some_and(|b| b.ca) {
                return Err(ChainError::ConstraintViolation(
                    "a CA certificate cannot be the end entity",
                ));
            }
            if key_usage.as_ref().is_some_and(|k| !k.digital_signature()) {
                return Err(ChainError::ConstraintViolation(
                    "end-entity KeyUsage does not permit digitalSignature",
                ));
            }
        }
        Role::Intermediate { below } => {
            let b = basic.ok_or(ChainError::ConstraintViolation(
                "intermediate carries no BasicConstraints",
            ))?;
            if !b.ca {
                return Err(ChainError::ConstraintViolation("intermediate is not a CA"));
            }
            if let Some(limit) = b.path_len_constraint
                && below > usize::from(limit)
            {
                return Err(ChainError::ConstraintViolation(
                    "pathLenConstraint exceeded",
                ));
            }
            if !key_usage.as_ref().is_some_and(|k| k.key_cert_sign()) {
                return Err(ChainError::ConstraintViolation(
                    "intermediate KeyUsage does not permit keyCertSign",
                ));
            }
        }
    }

    Ok(out)
}

/// `.1.4` epoch: canonical DER INTEGER, non-negative, fits u32.
fn parse_epoch(value: &[u8]) -> Result<u32, ChainError> {
    let int = der::asn1::Int::from_der(value)
        .map_err(|_| ChainError::BadExtension("epoch is not a DER INTEGER"))?;
    let raw = int.as_bytes();
    if raw.is_empty() || raw.len() > 5 {
        return Err(ChainError::BadExtension("epoch out of range"));
    }
    if raw[0] & 0x80 != 0 {
        return Err(ChainError::BadExtension("epoch is negative"));
    }
    let stripped: &[u8] = if raw[0] == 0x00 && raw.len() > 1 {
        &raw[1..]
    } else {
        raw
    };
    if stripped.len() > 4 {
        return Err(ChainError::BadExtension("epoch > u32"));
    }
    let mut buf = [0u8; 4];
    buf[4 - stripped.len()..].copy_from_slice(stripped);
    Ok(u32::from_be_bytes(buf))
}

fn parse_utf8(value: &[u8], what: &'static str) -> Result<String, ChainError> {
    der::asn1::Utf8StringRef::from_der(value)
        .map(|s| s.as_str().to_string())
        .map_err(|_| ChainError::BadExtension(what))
}

/// `.1.7` confinementDigest (bead `ley-line-open-c79ea8`): a DER OctetString
/// of exactly 32 bytes. Any other length rejects rather than truncating, so
/// a mis-encoded cert cannot satisfy a downstream identity-commit check.
fn parse_confinement_digest(value: &[u8]) -> Result<[u8; 32], ChainError> {
    let octets = der::asn1::OctetStringRef::from_der(value)
        .map_err(|_| ChainError::BadExtension("confinementDigest is not DER OctetString"))?;
    octets.as_bytes().try_into().map_err(|_| {
        ChainError::BadExtension("confinementDigest must be exactly 32 bytes (BLAKE3-256)")
    })
}

/// Hand-rolled JSON encoding of CertClaims, written by the FFI exports.
///
///   {"epk":"<base64>","nb":1234,"na":5678,"pr":"signet","ep":7,"pf":"...","sc":"...","cd":"<base64>"}
///
/// `pr` is emitted only for [`CertProfile::Signet`]; its absence means the
/// Interlace profile, so Interlace claims serialize to the same bytes as
/// before `pr` existed. `ep`, `pf`, `sc`, `cd` are emitted only when present
/// (`pf` and `sc` are never present under the Signet profile). `cd` is
/// base64url-nopad of the 32-byte confinementDigest.
pub fn claims_to_json(claims: &CertClaims) -> String {
    let mut out = String::with_capacity(256);
    out.push('{');
    out.push_str("\"epk\":\"");
    out.push_str(&base64_url_encode_no_pad(&claims.ephemeral_pubkey));
    out.push('"');

    out.push_str(",\"nb\":");
    out.push_str(&claims.not_before.to_string());

    out.push_str(",\"na\":");
    out.push_str(&claims.not_after.to_string());

    if claims.profile != CertProfile::Interlace {
        out.push_str(",\"pr\":\"");
        out.push_str(claims.profile.as_str());
        out.push('"');
    }

    if let Some(ep) = claims.epoch {
        out.push_str(",\"ep\":");
        out.push_str(&ep.to_string());
    }

    if let Some(ref pf) = claims.peer_fp {
        out.push_str(",\"pf\":\"");
        json_escape_into(pf, &mut out);
        out.push('"');
    }

    if let Some(ref sc) = claims.scope {
        out.push_str(",\"sc\":\"");
        json_escape_into(sc, &mut out);
        out.push('"');
    }

    if let Some(ref cd) = claims.confinement_digest {
        out.push_str(",\"cd\":\"");
        out.push_str(&base64_url_encode_no_pad(cd));
        out.push('"');
    }

    out.push('}');
    out
}

/// JSON string escape per RFC 8259 §7. Handles the required escapes
/// only (`"`, `\`, control chars). Non-ASCII passes through as UTF-8.
fn json_escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// Base64URL no-padding encoding. Used for `ephemeral_pubkey` so the JS
/// side can `b64decode` straight to a Uint8Array without padding fiddling.
fn base64_url_encode_no_pad(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHA[((n) & 0x3F) as usize] as char);
        }
    }
    out
}

/// Test helpers — minting fixtures for cert verification. `pub` because
/// integration tests and downstream dev tools (cloister's mint-dev-cert,
/// harness-sandbox) link the crate as a normal dependency. Intentionally
/// permissive (panics on bad inputs); every `.expect()` documents the
/// fixture invariant that must hold.
pub mod tests_helpers {
    use super::{oid_interlace, oid_signet};
    use crate::oid::ID_ED25519;
    use const_oid::ObjectIdentifier;
    use der::{
        Encode,
        asn1::{BitString, OctetString, Utf8StringRef},
    };
    use ed25519_dalek::{Signer, SigningKey};
    use std::str::FromStr;
    use x509_cert::{
        Certificate,
        certificate::TbsCertificate,
        ext::{
            Extension, Extensions,
            pkix::{BasicConstraints, KeyUsage, KeyUsages},
        },
        name::Name,
        serial_number::SerialNumber,
        spki::{AlgorithmIdentifier, SubjectPublicKeyInfo},
        time::{Time, Validity},
    };

    /// Canonical DER INTEGER for an unsigned u32.
    pub fn der_u32(v: u32) -> Vec<u8> {
        let raw = v.to_be_bytes();
        let mut start = 0;
        while start < 3 && raw[start] == 0 {
            start += 1;
        }
        let stripped = &raw[start..];
        let needs_pad = stripped[0] & 0x80 != 0;
        let content_len = stripped.len() + if needs_pad { 1 } else { 0 };
        let mut out = Vec::with_capacity(2 + content_len);
        out.push(0x02);
        out.push(content_len as u8);
        if needs_pad {
            out.push(0x00);
        }
        out.extend_from_slice(stripped);
        out
    }

    /// DER UTF8String.
    pub fn der_utf8(s: &str) -> Vec<u8> {
        Utf8StringRef::new(s)
            .expect("test fixture: valid UTF-8")
            .to_der()
            .expect("test fixture: Utf8String → DER")
    }

    /// DER BasicConstraints.
    pub fn der_basic_constraints(ca: bool, path_len: Option<u8>) -> Vec<u8> {
        BasicConstraints {
            ca,
            path_len_constraint: path_len,
        }
        .to_der()
        .expect("test fixture: BasicConstraints → DER")
    }

    /// DER KeyUsage with digitalSignature and/or keyCertSign (at least one).
    pub fn der_key_usage(digital_signature: bool, key_cert_sign: bool) -> Vec<u8> {
        let flags = match (digital_signature, key_cert_sign) {
            (true, true) => KeyUsages::DigitalSignature | KeyUsages::KeyCertSign,
            (true, false) => KeyUsages::DigitalSignature.into(),
            (false, true) => KeyUsages::KeyCertSign.into(),
            (false, false) => panic!("test fixture: KeyUsage needs at least one bit"),
        };
        KeyUsage(flags)
            .to_der()
            .expect("test fixture: KeyUsage → DER")
    }

    /// One extension: (OID, critical, extnValue contents).
    pub type RawExtension = (ObjectIdentifier, bool, Vec<u8>);

    /// Everything a fixture certificate varies on.
    pub struct CertSpec<'a> {
        pub issuer_key: &'a SigningKey,
        pub subject_key: &'a SigningKey,
        /// RFC 4514 string; empty for an empty Name.
        pub issuer: &'a str,
        pub subject: &'a str,
        pub not_before: i64,
        pub not_after: i64,
        pub extensions: Vec<RawExtension>,
    }

    fn name(s: &str) -> Name {
        if s.is_empty() {
            Name::default()
        } else {
            Name::from_str(s).expect("test fixture: RFC 4514 name parses")
        }
    }

    fn utc(secs: i64) -> Time {
        Time::UtcTime(
            der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(secs as u64))
                .expect("test fixture: time within UtcTime range"),
        )
    }

    /// Mint an Ed25519 certificate from `spec`. Returns DER bytes.
    pub fn mint_cert(spec: CertSpec<'_>) -> Vec<u8> {
        let alg_ed25519 = AlgorithmIdentifier {
            oid: ID_ED25519,
            parameters: None,
        };
        let spki = SubjectPublicKeyInfo {
            algorithm: alg_ed25519.clone(),
            subject_public_key: BitString::from_bytes(spec.subject_key.verifying_key().as_bytes())
                .expect("Ed25519 verifying key is 32 bytes — valid BIT STRING"),
        };
        let extensions: Vec<Extension> = spec
            .extensions
            .into_iter()
            .map(|(extn_id, critical, value)| Extension {
                extn_id,
                critical,
                extn_value: OctetString::new(value)
                    .expect("test fixture: extension value wraps into an OctetString"),
            })
            .collect();

        let tbs = TbsCertificate {
            version: x509_cert::Version::V3,
            serial_number: SerialNumber::new(&[1])
                .expect("test fixture: single-byte serial is valid"),
            signature: alg_ed25519.clone(),
            issuer: name(spec.issuer),
            validity: Validity {
                not_before: utc(spec.not_before),
                not_after: utc(spec.not_after),
            },
            subject: name(spec.subject),
            subject_public_key_info: spki,
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(
                    Extensions::try_from(extensions)
                        .expect("test fixture: extension list is well-formed"),
                )
            },
        };

        let tbs_der = tbs
            .to_der()
            .expect("test fixture: TbsCertificate encodes to DER");
        let sig = spec.issuer_key.sign(&tbs_der);
        Certificate {
            tbs_certificate: tbs,
            signature_algorithm: alg_ed25519,
            signature: BitString::from_bytes(&sig.to_bytes())
                .expect("test fixture: Ed25519 signature is 64 bytes"),
        }
        .to_der()
        .expect("test fixture: Certificate encodes to DER")
    }

    /// Interlace-profile extensions in the order cloister mints them.
    fn interlace_extensions(
        epoch: Option<u32>,
        peer_fp: Option<&str>,
        scope: Option<&str>,
        confinement_digest: Option<[u8; 32]>,
    ) -> Vec<RawExtension> {
        let mut exts = Vec::new();
        if let Some(ep) = epoch {
            exts.push((oid_interlace::EPOCH, false, der_u32(ep)));
        }
        if let Some(pf) = peer_fp {
            exts.push((oid_interlace::PEER_FP, false, der_utf8(pf)));
        }
        if let Some(sc) = scope {
            exts.push((oid_interlace::SCOPE, false, der_utf8(sc)));
        }
        if let Some(cd) = confinement_digest {
            let inner = OctetString::new(cd.to_vec())
                .expect("test fixture: 32-byte digest is a valid OctetString")
                .to_der()
                .expect("test fixture: OctetString → DER");
            exts.push((oid_interlace::CONFINEMENT_DIGEST, false, inner));
        }
        exts
    }

    /// Mint an Interlace-profile ephemeral cert signed by `master`. Empty
    /// issuer/subject Names, no standard extensions — the shape cloister's
    /// dev tooling mints.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_test_cert(
        master: &SigningKey,
        ephemeral: &SigningKey,
        not_before: i64,
        not_after: i64,
        epoch: Option<u32>,
        peer_fp: Option<&str>,
        scope: Option<&str>,
        confinement_digest: Option<[u8; 32]>,
    ) -> Vec<u8> {
        mint_cert(CertSpec {
            issuer_key: master,
            subject_key: ephemeral,
            issuer: "",
            subject: "",
            not_before,
            not_after,
            extensions: interlace_extensions(epoch, peer_fp, scope, confinement_digest),
        })
    }

    /// Interlace cert (epoch 7, peer `sha256:test`, scope `test:scope`) plus
    /// one extra extension. Used to test cloister-c71977's rejection of
    /// critical-flagged unknown extensions (RFC 5280 §4.2).
    pub fn mint_test_cert_with_extra_ext(
        master: &SigningKey,
        ephemeral: &SigningKey,
        not_before: i64,
        not_after: i64,
        extra_oid: ObjectIdentifier,
        extra_critical: bool,
        extra_value_der: Vec<u8>,
    ) -> Vec<u8> {
        let mut extensions =
            interlace_extensions(Some(7), Some("sha256:test"), Some("test:scope"), None);
        extensions.push((extra_oid, extra_critical, extra_value_der));
        mint_cert(CertSpec {
            issuer_key: master,
            subject_key: ephemeral,
            issuer: "",
            subject: "",
            not_before,
            not_after,
            extensions,
        })
    }

    /// The notme mint shape (worker/src/cert-authority.ts `mintBridgeCertPair`
    /// signing cert): critical BasicConstraints CA=false and KeyUsage
    /// digitalSignature, `.1.1` subject, `.1.2` issuance time, `.1.4` epoch,
    /// `.1.5` authMethod as DER UTF8String, `.1.6` 32 raw binding bytes.
    /// For unit tests; `tests/fixtures/notme` carries real notme output.
    pub fn signet_profile_leaf_extensions(epoch: u32, auth_method: &str) -> Vec<RawExtension> {
        vec![
            (
                const_oid::db::rfc5280::ID_CE_BASIC_CONSTRAINTS,
                true,
                der_basic_constraints(false, None),
            ),
            (
                const_oid::db::rfc5280::ID_CE_KEY_USAGE,
                true,
                der_key_usage(true, false),
            ),
            (oid_signet::SUBJECT, false, der_utf8("fixture")),
            (
                oid_signet::ISSUANCE_TIME,
                false,
                der_utf8("2026-10-27T00:00:00Z"),
            ),
            (oid_interlace::EPOCH, false, der_u32(epoch)),
            (oid_interlace::PEER_FP, false, der_utf8(auth_method)),
            (oid_interlace::SCOPE, false, vec![0xAB; 32]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::tests_helpers::{
        CertSpec, RawExtension, der_basic_constraints, der_key_usage, der_u32, der_utf8, mint_cert,
        mint_test_cert, mint_test_cert_with_extra_ext, signet_profile_leaf_extensions,
    };
    use super::*;
    use const_oid::db::rfc5280::{ID_CE_BASIC_CONSTRAINTS, ID_CE_KEY_USAGE};
    use ed25519_dalek::SigningKey;
    use rand::RngCore;
    use rand::rngs::OsRng;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// ed25519-dalek 3.x removed `SigningKey::generate` — 3.x requires an
    /// `rand_core 0.10`-compatible `CryptoRng`, and the sign crate keeps
    /// `rand = "0.8"` (rand_core 0.6) as a dev-dep until the workspace-wide
    /// rand bump lands (bead 3b2f55). We sidestep the trait-version mismatch
    /// entirely by seeding a `[u8; 32]` from rand 0.8's `OsRng` and handing
    /// raw bytes to `SigningKey::from_bytes`, which is infallible in 3.x.
    fn random_signing_key() -> SigningKey {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        SigningKey::from_bytes(&seed)
    }

    fn mint(issuer: &SigningKey, subject: &SigningKey, exts: Vec<RawExtension>) -> Vec<u8> {
        mint_named(issuer, subject, "", "", now(), now() + 300, exts)
    }

    fn mint_named(
        issuer_key: &SigningKey,
        subject_key: &SigningKey,
        issuer: &str,
        subject: &str,
        not_before: i64,
        not_after: i64,
        extensions: Vec<RawExtension>,
    ) -> Vec<u8> {
        mint_cert(CertSpec {
            issuer_key,
            subject_key,
            issuer,
            subject,
            not_before,
            not_after,
            extensions,
        })
    }

    fn ca_exts(path_len: Option<u8>, epoch: Option<u32>, signet: bool) -> Vec<RawExtension> {
        let mut v = vec![
            (
                ID_CE_BASIC_CONSTRAINTS,
                true,
                der_basic_constraints(true, path_len),
            ),
            (ID_CE_KEY_USAGE, true, der_key_usage(false, true)),
        ];
        if signet {
            v.push((oid_signet::SUBJECT, false, der_utf8("tier")));
        }
        if let Some(ep) = epoch {
            v.push((oid_interlace::EPOCH, false, der_u32(ep)));
        }
        v
    }

    #[test]
    fn happy_path_minimum_cert() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let nb = now();
        let na = nb + 300;
        let cert_der = mint_test_cert(&master, &ephemeral, nb, na, None, None, None, None);

        let claims = verify_cert_chain(&cert_der, master.verifying_key().as_bytes()).unwrap();

        assert_eq!(
            claims.ephemeral_pubkey,
            *ephemeral.verifying_key().as_bytes()
        );
        assert_eq!(claims.not_before, nb);
        assert_eq!(claims.not_after, na);
        assert_eq!(claims.profile, CertProfile::Interlace);
        assert!(claims.epoch.is_none());
        assert!(claims.peer_fp.is_none());
        assert!(claims.scope.is_none());
    }

    #[test]
    fn happy_path_with_interlace_extensions() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let nb = now();
        let na = nb + 300;
        let cert_der = mint_test_cert(
            &master,
            &ephemeral,
            nb,
            na,
            Some(7),
            Some("sha256:abc123"),
            Some("bead_create:/repos/foo"),
            None,
        );

        let claims = verify_cert_chain(&cert_der, master.verifying_key().as_bytes()).unwrap();

        assert_eq!(claims.profile, CertProfile::Interlace);
        assert_eq!(claims.epoch, Some(7));
        assert_eq!(claims.peer_fp.as_deref(), Some("sha256:abc123"));
        assert_eq!(claims.scope.as_deref(), Some("bead_create:/repos/foo"));
    }

    #[test]
    fn wrong_master_pubkey_rejects() {
        let master = random_signing_key();
        let other_master = random_signing_key();
        let ephemeral = random_signing_key();
        let nb = now();
        let cert_der = mint_test_cert(&master, &ephemeral, nb, nb + 300, None, None, None, None);

        let result = verify_cert_chain(&cert_der, other_master.verifying_key().as_bytes());
        assert!(matches!(result, Err(ChainError::BadSignature)));
    }

    #[test]
    fn truncated_cert_rejects() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert_der = mint_test_cert(
            &master,
            &ephemeral,
            now(),
            now() + 300,
            None,
            None,
            None,
            None,
        );

        let result = verify_cert_chain(
            &cert_der[..cert_der.len() / 2],
            master.verifying_key().as_bytes(),
        );
        assert!(matches!(result, Err(ChainError::BadDer(_))));
    }

    #[test]
    fn wrong_master_key_length_rejects() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert_der = mint_test_cert(
            &master,
            &ephemeral,
            now(),
            now() + 300,
            None,
            None,
            None,
            None,
        );

        let result = verify_cert_chain(&cert_der, &[0u8; 31]);
        assert!(matches!(result, Err(ChainError::BadMasterKey)));
    }

    // ── Claims JSON ──

    fn sample_claims(profile: CertProfile) -> CertClaims {
        CertClaims {
            ephemeral_pubkey: [0xAB; 32],
            not_before: 1,
            not_after: 2,
            profile,
            epoch: Some(7),
            peer_fp: Some("sha256:abc".to_string()),
            scope: Some("bead_create:*".to_string()),
            confinement_digest: None,
        }
    }

    #[test]
    fn claims_json_minimum_cert() {
        let claims = CertClaims {
            ephemeral_pubkey: [0xAB; 32],
            not_before: 1700000000,
            not_after: 1700000300,
            profile: CertProfile::Interlace,
            epoch: None,
            peer_fp: None,
            scope: None,
            confinement_digest: None,
        };
        let j = claims_to_json(&claims);
        assert!(j.contains("\"epk\":\"q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\""));
        assert!(j.contains("\"nb\":1700000000"));
        assert!(j.contains("\"na\":1700000300"));
        assert!(!j.contains("\"ep\""));
        assert!(!j.contains("\"pf\""));
        assert!(!j.contains("\"sc\""));
        assert!(!j.contains("\"pr\""));
    }

    /// Interlace claims serialize to exactly the bytes they did before the
    /// profile field existed — cloister's parser sees no change.
    #[test]
    fn claims_json_interlace_bytes_unchanged() {
        assert_eq!(
            claims_to_json(&sample_claims(CertProfile::Interlace)),
            "{\"epk\":\"q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\",\"nb\":1,\"na\":2,\
             \"ep\":7,\"pf\":\"sha256:abc\",\"sc\":\"bead_create:*\"}"
        );
    }

    #[test]
    fn claims_json_signet_profile_emits_pr() {
        let mut claims = sample_claims(CertProfile::Signet);
        claims.peer_fp = None;
        claims.scope = None;
        assert_eq!(
            claims_to_json(&claims),
            "{\"epk\":\"q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\",\"nb\":1,\"na\":2,\
             \"pr\":\"signet\",\"ep\":7}"
        );
    }

    #[test]
    fn claims_json_escapes_strings() {
        let mut claims = sample_claims(CertProfile::Interlace);
        claims.peer_fp = Some("a\"b\\c\n".to_string());
        let j = claims_to_json(&claims);
        assert!(j.contains("\"pf\":\"a\\\"b\\\\c\\n\""));
    }

    // ── Critical-extension rejection (cloister-c71977 / RFC 5280 §4.2) ──

    /// An OID under a private arc that the verifier doesn't know about.
    const UNKNOWN_PRIVATE_OID: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.42.1");

    #[test]
    fn rejects_critical_unknown_extension() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint_test_cert_with_extra_ext(
            &master,
            &ephemeral,
            now() - 60,
            now() + 600,
            UNKNOWN_PRIVATE_OID,
            true,
            vec![0x04, 0x01, 0x01],
        );

        let result = verify_cert_chain(&cert, master.verifying_key().as_bytes());
        match result {
            Err(ChainError::UnknownCriticalExtension(oid)) => {
                assert_eq!(oid, "1.3.6.1.4.1.99999.42.1");
            }
            _ => panic!("expected UnknownCriticalExtension, got {:?}", result),
        }
    }

    #[test]
    fn accepts_non_critical_unknown_extension() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint_test_cert_with_extra_ext(
            &master,
            &ephemeral,
            now() - 60,
            now() + 600,
            UNKNOWN_PRIVATE_OID,
            false,
            vec![0x04, 0x01, 0x01],
        );

        let claims = verify_cert_chain(&cert, master.verifying_key().as_bytes())
            .expect("non-critical unknown should be ignored");
        assert_eq!(claims.epoch, Some(7));
        assert_eq!(claims.peer_fp.as_deref(), Some("sha256:test"));
    }

    // ── Profile discrimination (bead ley-line-open-9ac66d) ──

    /// The notme leaf shape: `.1.5` holds authMethod as a DER UTF8String.
    /// Before the discriminator this parsed silently as peer_fp.
    #[test]
    fn signet_profile_never_reads_auth_method_as_peer_fp() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint(
            &master,
            &ephemeral,
            signet_profile_leaf_extensions(3, "passkey"),
        );

        let claims = verify_cert_chain(&cert, master.verifying_key().as_bytes())
            .expect("signet-profile leaf verifies");
        assert_eq!(claims.profile, CertProfile::Signet);
        assert_eq!(claims.epoch, Some(3));
        assert_eq!(claims.peer_fp, None);
        assert_eq!(claims.scope, None);
    }

    /// Each marker on its own selects the Signet profile.
    #[test]
    fn each_marker_selects_signet_profile() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        for marker in oid_signet::MARKERS {
            let cert = mint(
                &master,
                &ephemeral,
                vec![
                    (marker, false, der_utf8("x")),
                    (oid_interlace::PEER_FP, false, der_utf8("sha256:abc")),
                    (oid_interlace::SCOPE, false, der_utf8("bead_create:*")),
                ],
            );
            let claims = verify_cert_chain(&cert, master.verifying_key().as_bytes()).unwrap();
            assert_eq!(claims.profile, CertProfile::Signet, "marker {marker}");
            assert_eq!(claims.peer_fp, None, "marker {marker}");
            assert_eq!(claims.scope, None, "marker {marker}");
        }
    }

    /// `.1.5`/`.1.6` are not processed under the Signet profile, so a
    /// critical one is an unrecognized critical extension.
    #[test]
    fn signet_profile_rejects_critical_unread_arc_extension() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint(
            &master,
            &ephemeral,
            vec![
                (oid_signet::SUBJECT, false, der_utf8("x")),
                (oid_interlace::PEER_FP, true, der_utf8("passkey")),
            ],
        );
        assert!(matches!(
            verify_cert_chain(&cert, master.verifying_key().as_bytes()),
            Err(ChainError::UnknownCriticalExtension(oid)) if oid == "1.3.6.1.4.1.99999.1.5"
        ));
    }

    // ── Fail closed on malformed known extensions ──

    fn expect_bad_extension(exts: Vec<RawExtension>) {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint(&master, &ephemeral, exts);
        let r = verify_cert_chain(&cert, master.verifying_key().as_bytes());
        assert!(matches!(r, Err(ChainError::BadExtension(_))), "got {r:?}");
    }

    #[test]
    fn interlace_scope_raw_bytes_rejects() {
        // notme's 32 raw binding bytes, without any Signet marker.
        expect_bad_extension(vec![(oid_interlace::SCOPE, false, vec![0x17; 32])]);
    }

    #[test]
    fn non_canonical_epoch_rejects_in_both_profiles() {
        // notme before 466074f: fixed-width `02 04 00 00 00 01`.
        let old_notme_epoch = vec![0x02, 0x04, 0x00, 0x00, 0x00, 0x01];
        expect_bad_extension(vec![(oid_interlace::EPOCH, false, old_notme_epoch.clone())]);
        expect_bad_extension(vec![
            (oid_signet::SUBJECT, false, der_utf8("x")),
            (oid_interlace::EPOCH, false, old_notme_epoch),
        ]);
    }

    #[test]
    fn negative_epoch_rejects() {
        expect_bad_extension(vec![(oid_interlace::EPOCH, false, vec![0x02, 0x01, 0xFF])]);
    }

    #[test]
    fn malformed_standard_extensions_reject() {
        expect_bad_extension(vec![(ID_CE_BASIC_CONSTRAINTS, true, vec![0x04, 0x00])]);
        expect_bad_extension(vec![(ID_CE_KEY_USAGE, true, vec![0x0C, 0x00])]);
        expect_bad_extension(vec![(
            const_oid::db::rfc5280::ID_CE_SUBJECT_ALT_NAME,
            true,
            vec![0x02, 0x01, 0x00],
        )]);
    }

    #[test]
    fn duplicate_extension_rejects() {
        expect_bad_extension(vec![
            (oid_interlace::PEER_FP, false, der_utf8("sha256:a")),
            (oid_interlace::PEER_FP, false, der_utf8("sha256:b")),
        ]);
    }

    // ── End-entity constraints ──

    #[test]
    fn ca_certificate_as_end_entity_rejects() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint(&master, &ephemeral, ca_exts(Some(0), None, true));
        assert!(matches!(
            verify_cert_chain(&cert, master.verifying_key().as_bytes()),
            Err(ChainError::ConstraintViolation(_))
        ));
    }

    #[test]
    fn end_entity_key_usage_without_digital_signature_rejects() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let cert = mint(
            &master,
            &ephemeral,
            vec![(ID_CE_KEY_USAGE, true, der_key_usage(false, true))],
        );
        assert!(matches!(
            verify_cert_chain(&cert, master.verifying_key().as_bytes()),
            Err(ChainError::ConstraintViolation(_))
        ));
    }

    // ── Paths ──

    struct Path {
        root: SigningKey,
        tier: SigningKey,
        leaf_key: SigningKey,
        tier_der: Vec<u8>,
        leaf_der: Vec<u8>,
    }

    fn signet_path(tier_epoch: u32, leaf_epoch: u32) -> Path {
        let root = random_signing_key();
        let tier = random_signing_key();
        let leaf_key = random_signing_key();
        let tier_der = mint_named(
            &root,
            &tier,
            "CN=signet-authority,O=notme",
            "CN=machine,O=notme",
            100,
            10_000,
            ca_exts(Some(0), Some(tier_epoch), true),
        );
        let leaf_der = mint_named(
            &tier,
            &leaf_key,
            "CN=machine,O=notme",
            "CN=task,O=notme",
            200,
            20_000,
            signet_profile_leaf_extensions(leaf_epoch, "passkey"),
        );
        Path {
            root,
            tier,
            leaf_key,
            tier_der,
            leaf_der,
        }
    }

    #[test]
    fn path_verifies_and_intersects_validity() {
        let p = signet_path(5, 5);
        let claims = verify_cert_path(
            &p.leaf_der,
            &[&p.tier_der],
            p.root.verifying_key().as_bytes(),
        )
        .expect("path verifies");
        assert_eq!(
            claims.ephemeral_pubkey,
            *p.leaf_key.verifying_key().as_bytes()
        );
        assert_eq!(claims.not_before, 200);
        assert_eq!(claims.not_after, 10_000);
        assert_eq!(claims.epoch, Some(5));
        assert_eq!(claims.profile, CertProfile::Signet);
        assert_eq!(claims.peer_fp, None);

        // The leaf alone does not chain to the root.
        assert!(matches!(
            verify_cert_chain(&p.leaf_der, p.root.verifying_key().as_bytes()),
            Err(ChainError::BadSignature)
        ));
        // The leaf alone verifies under the tier key.
        verify_cert_chain(&p.leaf_der, p.tier.verifying_key().as_bytes()).unwrap();
    }

    #[test]
    fn path_epoch_disagreement_rejects() {
        let p = signet_path(4, 5);
        assert!(matches!(
            verify_cert_path(
                &p.leaf_der,
                &[&p.tier_der],
                p.root.verifying_key().as_bytes()
            ),
            Err(ChainError::ConstraintViolation(
                "epoch differs across the path"
            ))
        ));
    }

    #[test]
    fn path_issuer_name_mismatch_rejects() {
        let p = signet_path(5, 5);
        let wrong_issuer = mint_named(
            &p.tier,
            &p.leaf_key,
            "CN=signet-authority,O=notme",
            "CN=task,O=notme",
            200,
            20_000,
            signet_profile_leaf_extensions(5, "passkey"),
        );
        assert!(matches!(
            verify_cert_path(
                &wrong_issuer,
                &[&p.tier_der],
                p.root.verifying_key().as_bytes()
            ),
            Err(ChainError::ConstraintViolation(_))
        ));
    }

    #[test]
    fn path_intermediate_must_be_ca_with_key_cert_sign() {
        let root = random_signing_key();
        let tier = random_signing_key();
        let leaf = random_signing_key();
        let leaf_der = mint(&tier, &leaf, vec![]);
        for tier_exts in [
            vec![],
            vec![(
                ID_CE_BASIC_CONSTRAINTS,
                true,
                der_basic_constraints(false, None),
            )],
            vec![(
                ID_CE_BASIC_CONSTRAINTS,
                true,
                der_basic_constraints(true, None),
            )],
            vec![
                (
                    ID_CE_BASIC_CONSTRAINTS,
                    true,
                    der_basic_constraints(true, None),
                ),
                (ID_CE_KEY_USAGE, true, der_key_usage(true, false)),
            ],
        ] {
            let tier_der = mint(&root, &tier, tier_exts);
            assert!(matches!(
                verify_cert_path(&leaf_der, &[&tier_der], root.verifying_key().as_bytes()),
                Err(ChainError::ConstraintViolation(_))
            ));
        }
    }

    #[test]
    fn path_len_constraint_enforced() {
        let root = random_signing_key();
        let upper = random_signing_key();
        let lower = random_signing_key();
        let leaf = random_signing_key();
        let leaf_der = mint(&lower, &leaf, vec![]);
        let lower_der = mint(&upper, &lower, ca_exts(Some(0), None, false));
        for (limit, ok) in [(Some(0u8), false), (Some(1), true), (None, true)] {
            let upper_der = mint(&root, &upper, ca_exts(limit, None, false));
            let r = verify_cert_path(
                &leaf_der,
                &[&lower_der, &upper_der],
                root.verifying_key().as_bytes(),
            );
            assert_eq!(r.is_ok(), ok, "pathLen {limit:?}: {r:?}");
        }
    }

    #[test]
    fn path_profile_disagreement_rejects() {
        let root = random_signing_key();
        let tier = random_signing_key();
        let leaf = random_signing_key();
        let tier_der = mint(&root, &tier, ca_exts(None, None, true));
        let leaf_der = mint(&tier, &leaf, vec![]);
        assert!(matches!(
            verify_cert_path(&leaf_der, &[&tier_der], root.verifying_key().as_bytes()),
            Err(ChainError::ConstraintViolation(_))
        ));
    }

    #[test]
    fn path_too_many_intermediates_rejects() {
        let root = random_signing_key();
        let leaf = random_signing_key();
        let leaf_der = mint(&root, &leaf, vec![]);
        let many: Vec<&[u8]> = vec![&leaf_der[..]; MAX_INTERMEDIATES + 1];
        assert!(matches!(
            verify_cert_path(&leaf_der, &many, root.verifying_key().as_bytes()),
            Err(ChainError::ConstraintViolation(_))
        ));
    }

    #[test]
    fn split_concatenated_der_round_trips_and_rejects_garbage() {
        let p = signet_path(1, 1);
        let mut buf = p.tier_der.clone();
        buf.extend_from_slice(&p.leaf_der);
        let parts = split_concatenated_der(&buf).unwrap();
        assert_eq!(parts, vec![&p.tier_der[..], &p.leaf_der[..]]);
        assert!(split_concatenated_der(&[]).unwrap().is_empty());
        buf.push(0x30);
        assert!(matches!(
            split_concatenated_der(&buf),
            Err(ChainError::BadDer(_))
        ));
    }

    // ── Wire-format preservation gates (ed25519-dalek 3.x bump, bead 474c0a) ──

    /// Fixed-seed round-trip: same seed → same key bytes → same signature.
    /// Guards against a keygen semantics change slipping in with future
    /// ed25519-dalek bumps. If this test breaks, `SigningKey::from_bytes`
    /// no longer treats its 32-byte input as the seed defined by RFC 8032.
    #[test]
    fn fixed_seed_produces_stable_key_bytes() {
        // Deterministic 32-byte seed — hand-picked so a regression in
        // `from_bytes` (e.g. reinterpretation as scalar rather than seed)
        // would flip both derived pubkey and signature outputs.
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        // Expected pubkey is the RFC 8032 §7.1 test-vector pair for this
        // seed: SHA-512(seed) → clamp → scalar → * G. Any keygen change
        // that broke wire-format compatibility with 2.x would flip these.
        let expected_pubkey: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let key = SigningKey::from_bytes(&seed);
        assert_eq!(
            key.verifying_key().as_bytes(),
            &expected_pubkey,
            "keygen from RFC 8032 §7.1 test seed produced unexpected pubkey — \
             wire format may have shifted",
        );

        // Signing the RFC 8032 §7.1 empty-message test vector gives the
        // exact signature bytes below. Any ed25519 wire-format change
        // would flip these.
        use ed25519_dalek::Signer;
        let sig = key.sign(b"");
        let expected_sig: [u8; 64] = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];
        assert_eq!(
            sig.to_bytes(),
            expected_sig,
            "RFC 8032 §7.1 test-vector signature mismatch — wire format \
             changed under the bump",
        );

        // Verify round-trip closes.
        key.verifying_key()
            .verify(b"", &sig)
            .expect("self-signature should verify");
    }

    /// A signature produced under one instance must verify under a
    /// separately-constructed VerifyingKey with the same pubkey bytes.
    /// Guards against a "verifying key is opaque, not deterministic from
    /// bytes" regression.
    #[test]
    fn cross_instance_verify_round_trip() {
        let signer = random_signing_key();
        let msg = b"leyline-sign round-trip gate";
        use ed25519_dalek::Signer;
        let sig = signer.sign(msg);

        // Rebuild the verifying key from raw bytes on the "verifier" side.
        let pubkey_bytes = *signer.verifying_key().as_bytes();
        let verifier = VerifyingKey::from_bytes(&pubkey_bytes)
            .expect("VerifyingKey::from_bytes on valid pubkey");
        verifier
            .verify(msg, &sig)
            .expect("cross-instance verify must succeed");

        // And a wrong message must fail.
        assert!(
            verifier.verify(b"tampered", &sig).is_err(),
            "wrong-message verify must fail",
        );
    }
}
