//! Operator-configured evidence verification for the execution daemon (bead
//! `ley-line-open-410921`).
//!
//! [`CasDsseEvidenceVerifier`] needs two things LLO does not own: an evidence
//! store holding the DSSE envelopes a grant references, and the trust keys
//! those envelopes and the grant must be signed by. An embedding authority
//! (Cloister) supplies its own. This module is what a standalone `leyline
//! execution-daemon` uses instead: [`CasEvidenceStore`] over LLO's filesystem
//! CAS blob store, and trust keys read from operator-supplied files. LLO
//! ships no trust keys; with none configured the daemon rejects every grant.
//!
//! Three identity domains meet here and must not be conflated:
//!
//! | Name | Names | Construction |
//! | --- | --- | --- |
//! | kid ([`TrustKey::kid`]) | one Ed25519 trust key | ADR-012: `lowercasehex(SHA-256(canonical SPKI DER)[:16])` |
//! | evidence digest | one stored DSSE envelope | `blake3-256:` + BLAKE3 over the exact envelope bytes |
//! | run binding | the run the evidence authorizes | in-toto subject digest `blake3` = derived `run-<hex>` |
//!
//! A kid is a key's public name and never addresses content; an evidence
//! digest addresses content and never names a key. The store refuses any
//! digest outside its own domain.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use leyline_core::{BlobStore as _, FsBlobStore, Hash};
use leyline_envelope::VerifyingKey;

use crate::ExecutionError;
use crate::authorization::{CasDsseEvidenceVerifier, EvidenceRejection, EvidenceStore};

/// Prefix of an evidence digest — the blob-hash domain.
const EVIDENCE_DIGEST_PREFIX: &str = "blake3-256:";

/// RFC 8410 Ed25519 `SubjectPublicKeyInfo` prefix (absent parameters): the
/// only canonical Ed25519 SPKI DER, 12 bytes before the 32-byte key.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// A misconfigured operator verifier. Every variant refuses daemon startup.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceConfigError {
    #[error("evidence store {path} is not an existing directory")]
    EvidenceStore { path: PathBuf },
    #[error("no trust keys configured: an evidence store needs at least one --trust-key")]
    NoTrustKeys,
    #[error("trust key {path}: {reason}")]
    TrustKey { path: PathBuf, reason: String },
    #[error("trust key {path} repeats kid {kid}")]
    DuplicateTrustKey { path: PathBuf, kid: String },
}

/// An operator trust key and its ADR-012 canonical kid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustKey {
    pub key: VerifyingKey,
    /// `lowercasehex(SHA-256(canonical SPKI DER)[:16])` — the key's public
    /// name. Used to label rejections and log the trust set; it never
    /// selects a key for verification.
    pub kid: String,
}

impl TrustKey {
    pub fn new(key: VerifyingKey) -> Self {
        Self {
            kid: leyline_sign::kid::canonical_kid(&key),
            key,
        }
    }
}

/// Parse one trust key file.
///
/// Two encodings are accepted, each holding exactly one Ed25519 public key:
///
/// - 64 hex characters — the raw 32-byte key;
/// - a PEM `PUBLIC KEY` block whose DER is the canonical RFC 8410 SPKI.
///
/// Anything else — a private key, a non-canonical SPKI (for instance NULL
/// parameters), a second key, trailing material — is refused rather than
/// guessed at: trust material is operator configuration.
pub fn parse_trust_key(contents: &[u8]) -> Result<TrustKey, String> {
    let text = std::str::from_utf8(contents)
        .map_err(|_| "trust key file is not UTF-8".to_owned())?
        .trim();
    if text.is_empty() {
        return Err("trust key file is empty".into());
    }
    let raw: [u8; 32] = if text.starts_with("-----BEGIN") {
        let der = pem_public_key_der(text)?;
        let key = der
            .strip_prefix(&ED25519_SPKI_PREFIX[..])
            .filter(|key| key.len() == 32)
            .ok_or_else(|| {
                "PUBLIC KEY is not a canonical RFC 8410 Ed25519 SubjectPublicKeyInfo".to_owned()
            })?;
        key.try_into().expect("length checked")
    } else {
        if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("trust key must be 64 hex characters or a PEM PUBLIC KEY block".to_owned());
        }
        let mut raw = [0u8; 32];
        for (index, byte) in raw.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(|error| format!("trust key is not hex: {error}"))?;
        }
        raw
    };
    let key = VerifyingKey::from_bytes(&raw)
        .map_err(|error| format!("not a valid Ed25519 public key: {error}"))?;
    Ok(TrustKey::new(key))
}

/// The DER inside exactly one PEM `PUBLIC KEY` block that is the whole input.
fn pem_public_key_der(text: &str) -> Result<Vec<u8>, String> {
    const BEGIN: &str = "-----BEGIN PUBLIC KEY-----";
    const END: &str = "-----END PUBLIC KEY-----";
    let body = text
        .strip_prefix(BEGIN)
        .and_then(|rest| rest.strip_suffix(END))
        .ok_or_else(|| "PEM block must be exactly one PUBLIC KEY".to_owned())?;
    if body.contains("-----") {
        return Err("PEM block must be exactly one PUBLIC KEY".into());
    }
    let base64: String = body.split_whitespace().collect();
    base64::engine::general_purpose::STANDARD
        .decode(base64.as_bytes())
        .map_err(|error| format!("PUBLIC KEY is not base64: {error}"))
}

/// Read and parse one trust key file.
pub fn load_trust_key(path: &Path) -> Result<TrustKey, EvidenceConfigError> {
    let contents = std::fs::read(path).map_err(|error| EvidenceConfigError::TrustKey {
        path: path.to_owned(),
        reason: error.to_string(),
    })?;
    parse_trust_key(&contents).map_err(|reason| EvidenceConfigError::TrustKey {
        path: path.to_owned(),
        reason,
    })
}

/// Read every trust key file, refusing an empty set and a repeated key.
pub fn load_trust_keys(paths: &[PathBuf]) -> Result<Vec<TrustKey>, EvidenceConfigError> {
    if paths.is_empty() {
        return Err(EvidenceConfigError::NoTrustKeys);
    }
    let mut seen = BTreeSet::new();
    let mut keys = Vec::with_capacity(paths.len());
    for path in paths {
        let key = load_trust_key(path)?;
        if !seen.insert(key.kid.clone()) {
            return Err(EvidenceConfigError::DuplicateTrustKey {
                path: path.clone(),
                kid: key.kid,
            });
        }
        keys.push(key);
    }
    Ok(keys)
}

/// DSSE evidence held in LLO's filesystem CAS blob store
/// ([`FsBlobStore`]: `<root>/<aa>/<62 hex>`, BLAKE3-addressed, verified on
/// every read).
#[derive(Debug)]
pub struct CasEvidenceStore {
    blobs: FsBlobStore,
}

impl CasEvidenceStore {
    /// Open an existing store directory. It is never created: a mistyped
    /// path must refuse startup, not serve an empty store.
    pub fn open(root: &Path) -> Result<Self, EvidenceConfigError> {
        if !root.is_dir() {
            return Err(EvidenceConfigError::EvidenceStore {
                path: root.to_owned(),
            });
        }
        let blobs = FsBlobStore::new(root).map_err(|_| EvidenceConfigError::EvidenceStore {
            path: root.to_owned(),
        })?;
        Ok(Self { blobs })
    }

    pub fn root(&self) -> &Path {
        self.blobs.root()
    }

    /// Store envelope bytes, returning their evidence digest
    /// (`blake3-256:<hex>`) — the value a `RunGrant` `EvidenceRef` carries.
    pub fn insert(&self, bytes: &[u8]) -> anyhow::Result<String> {
        // `put` takes `&mut self` by trait contract only; the store's sole
        // state is its root, so a second handle writes the same objects.
        let mut writer = FsBlobStore::new(self.blobs.root())?;
        Ok(format!("{EVIDENCE_DIGEST_PREFIX}{}", writer.put(bytes)?))
    }
}

/// The 32-byte hash an evidence digest names, if it is in the domain.
fn parse_evidence_digest(digest: &str) -> Option<Hash> {
    let hex = digest.strip_prefix(EVIDENCE_DIGEST_PREFIX)?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let mut raw = [0u8; 32];
    for (index, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(Hash::from_bytes(raw))
}

impl EvidenceStore for CasEvidenceStore {
    fn load(&self, digest: &str) -> Result<Vec<u8>, ExecutionError> {
        let hash = parse_evidence_digest(digest).ok_or_else(|| {
            EvidenceRejection::EvidenceDigestMalformed
                .error(format!("{digest:?} is not a blake3-256 evidence digest"))
        })?;
        match self.blobs.get(hash) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(EvidenceRejection::EvidenceMissing
                .error(format!("no evidence stored under {digest}"))),
            // Verify-on-read failed, or the blob could not be read: either
            // way the store does not vouch for these bytes.
            Err(error) => Err(EvidenceRejection::EvidenceIntegrity.error(format!(
                "evidence {digest} is not readable as stored: {error:#}"
            ))),
        }
    }
}

/// The verifier `leyline execution-daemon --evidence-store DIR --trust-key
/// FILE...` installs: [`CasDsseEvidenceVerifier`] over [`CasEvidenceStore`],
/// trusting exactly the supplied keys.
///
/// Fails closed on every configuration error: a missing store directory, no
/// trust keys, an unreadable or malformed key, a key listed twice.
pub fn operator_verifier(
    evidence_store: &Path,
    trust_key_paths: &[PathBuf],
) -> Result<CasDsseEvidenceVerifier<CasEvidenceStore>, EvidenceConfigError> {
    let keys = load_trust_keys(trust_key_paths)?;
    let store = CasEvidenceStore::open(evidence_store)?;
    Ok(CasDsseEvidenceVerifier::new(
        Arc::new(store),
        keys.into_iter().map(|key| key.key).collect(),
    ))
}
