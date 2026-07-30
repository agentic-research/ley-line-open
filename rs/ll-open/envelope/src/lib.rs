//! DSSE (Dead Simple Signing Envelope) + in-toto Statement v1 — the
//! substrate's attestation mechanism, signing composed over
//! [`leyline_sign`]'s root signer.
//!
//! This crate is the byte-compatible hoist of rosary's `src/dsse.rs` (bead
//! `ley-line-open-319a08`): identical `(statement, key)` inputs produce
//! identical payload and signature bytes, proven by the pinned vector in the
//! test module. Consumers keep only *policy* — what to attest, when a key is
//! configured — and hand the mechanism a [`Statement`] plus a signer.
//!
//! ## Wire format
//!
//! ```json
//! {
//!   "payloadType": "application/vnd.in-toto+json",
//!   "payload": "<base64url(in-toto Statement JSON)>",
//!   "signatures": [{ "keyid": "<hint>", "sig": "<base64url(sig)>" }]
//! }
//! ```
//!
//! The in-toto Statement is the payload:
//!
//! ```json
//! {
//!   "_type": "https://in-toto.io/Statement/v1",
//!   "subject": [{ "name": "...", "digest": { "sha256": "<hex>" } }],
//!   "predicateType": "...",
//!   "predicate": { }
//! }
//! ```
//!
//! ## PAE (Pre-Authentication Encoding)
//!
//! `PAE(type, body) = "DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`
//!
//! Per the DSSE spec the Ed25519 signature covers PAE applied to the **raw**
//! statement bytes — never the base64 text — so any external DSSE verifier
//! can check it by decoding the payload and reconstructing PAE.
//!
//! ## keyid: emission vs acceptance (migration posture)
//!
//! DSSE's `keyid` is an **unauthenticated hint** — it is not covered by the
//! signature, so verification never trusts it. [`Envelope::verify`] verifies
//! the supplied key against every signature regardless of what the `keyid`
//! claims (the same parity-not-lookup rule as ADR-012 / R1 in
//! `leyline_sign::root_signer::verify_head`).
//!
//! - **Emission**: new envelopes write the ecosystem-canonical kid —
//!   `leyline_sign::kid::canonical_kid`, ADR-012's
//!   `lowercasehex(SHA-256(SPKI DER)[:16])`.
//! - **Acceptance**: envelopes carrying the legacy rosary keyid —
//!   `hex(sha256(raw pubkey))`, 64 hex chars — or any other hint, or none at
//!   all, verify forever. Since the hint is never consulted, legacy envelopes
//!   need no re-signing and no translation.
//!
//! ## Unsigned forensic records
//!
//! When no key is configured, the forensic record is a raw in-toto Statement
//! — [`UnsignedStatement`], a distinct type — never a DSSE envelope with an
//! empty `signatures` array. [`Envelope`] construction refuses empty
//! signatures, so "unsigned envelope" is unrepresentable.

use base64::Engine as _;
use ed25519_dalek::Verifier as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

// The signer surface this crate composes over, re-exported so consumers need
// no direct leyline-sign dependency for the common sign/verify path.
pub use leyline_sign::root_signer::{Ed25519RootSigner, VerifyingKey};

/// DSSE `payloadType` for an in-toto Statement payload.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// in-toto Statement v1 `_type`.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Shape violations caught at construction — a value of type [`Envelope`] or
/// [`Statement`] cannot exist in these states (parse, don't validate).
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("payloadType must be {PAYLOAD_TYPE:?}, got {0:?}")]
    WrongPayloadType(String),
    #[error("statement _type must be {STATEMENT_TYPE:?}, got {0:?}")]
    WrongStatementType(String),
    #[error(
        "envelope has no signatures — an unsigned record is an UnsignedStatement, \
         never an empty-signature envelope"
    )]
    NoSignatures,
    #[error("payload is not canonical base64url (no pad): {0}")]
    PayloadEncoding(base64::DecodeError),
    #[error("signature must be base64url (no pad) of exactly 64 bytes")]
    SignatureEncoding,
    #[error("malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Verification refusals. Anything other than `Ok` means the envelope must
/// not be trusted — there is no "partially valid" outcome.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("signature does not verify under the supplied key")]
    SignatureInvalid,
    /// The signature verified but the authenticated payload is not a valid
    /// in-toto Statement. (Signature is checked *first* — the parser only
    /// ever runs on authenticated bytes.)
    #[error("authenticated payload is not a valid statement: {0}")]
    Payload(#[from] ParseError),
}

// ---------------------------------------------------------------------------
// PAE encoding
// ---------------------------------------------------------------------------

/// Pre-Authentication Encoding per the DSSE spec:
/// `PAE(type, body) = "DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`.
///
/// Lengths are byte counts in ASCII decimal. Because both lengths are framed
/// into the encoding, `PAE(t, b1)` is never a prefix of `PAE(t, b2)` for
/// `b1 != b2` — the property that makes the signature non-malleable across
/// payload boundaries (falsified over a generated corpus in the tests).
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

// ---------------------------------------------------------------------------
// Wire structs (private): field ORDER is the rosary byte-compat contract —
// serde_json emits struct fields in declaration order, and the pinned vector
// in the tests notarizes it.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct EnvelopeWire {
    #[serde(rename = "payloadType")]
    payload_type: String,
    payload: String,
    signatures: Vec<SignatureWire>,
}

#[derive(Serialize, Deserialize)]
struct SignatureWire {
    // Absent keyid deserializes to "" and an empty keyid serializes to
    // nothing — exactly rosary's wire behavior, so parse→serialize is
    // byte-identity on legacy envelopes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    keyid: String,
    sig: String,
}

#[derive(Serialize, Deserialize)]
struct StatementWire {
    #[serde(rename = "_type")]
    statement_type: String,
    subject: Vec<SubjectWire>,
    #[serde(rename = "predicateType")]
    predicate_type: String,
    predicate: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct SubjectWire {
    name: String,
    // BTreeMap, not HashMap: deterministic serialization when a subject ever
    // carries more than one digest algorithm. Byte-identical to rosary for
    // the single-entry `sha256` case.
    digest: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Statement
// ---------------------------------------------------------------------------

/// One in-toto subject: a named artifact and its digest set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    name: String,
    digest: BTreeMap<String, String>,
}

impl Subject {
    /// Subject whose `sha256` digest is computed here, from the artifact's
    /// exact bytes — callers hand over bytes, not hex, so a subject can never
    /// hold a digest that doesn't match what was hashed. For a file subject
    /// those MUST be the on-disk bytes (an external observer verifies by
    /// hashing the file, not by re-serializing your in-memory value).
    pub fn sha256_of(name: impl Into<String>, artifact_bytes: &[u8]) -> Self {
        let mut digest = BTreeMap::new();
        digest.insert(
            "sha256".to_string(),
            hex::encode(Sha256::digest(artifact_bytes)),
        );
        Self {
            name: name.into(),
            digest,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `sha256` digest as lowercase hex, if this subject carries one
    /// (foreign statements may digest under other algorithms).
    pub fn sha256_hex(&self) -> Option<&str> {
        self.digest.get("sha256").map(String::as_str)
    }

    fn from_wire(w: SubjectWire) -> Self {
        Self {
            name: w.name,
            digest: w.digest,
        }
    }

    fn to_wire(&self) -> SubjectWire {
        SubjectWire {
            name: self.name.clone(),
            digest: self.digest.clone(),
        }
    }
}

/// An in-toto Statement v1. The `_type` field is not stored — it is a
/// constant of the type, injected on serialization and demanded on parse, so
/// a `Statement` with the wrong `_type` is unrepresentable.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    subject: Vec<Subject>,
    predicate_type: String,
    predicate: serde_json::Value,
}

impl Statement {
    pub fn new(
        subject: Vec<Subject>,
        predicate_type: impl Into<String>,
        predicate: serde_json::Value,
    ) -> Self {
        Self {
            subject,
            predicate_type: predicate_type.into(),
            predicate,
        }
    }

    pub fn subject(&self) -> &[Subject] {
        &self.subject
    }

    pub fn predicate_type(&self) -> &str {
        &self.predicate_type
    }

    pub fn predicate(&self) -> &serde_json::Value {
        &self.predicate
    }

    /// Compact statement JSON — the exact bytes that get base64url-encoded
    /// into an envelope payload and covered by the signature.
    pub fn to_json_vec(&self) -> Vec<u8> {
        let wire = StatementWire {
            statement_type: STATEMENT_TYPE.to_string(),
            subject: self.subject.iter().map(Subject::to_wire).collect(),
            predicate_type: self.predicate_type.clone(),
            predicate: self.predicate.clone(),
        };
        // Infallible: string-keyed maps and JSON values only.
        serde_json::to_vec(&wire).expect("statement serialization cannot fail")
    }

    /// Parse statement JSON, refusing anything whose `_type` is not the
    /// in-toto Statement v1 marker.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, ParseError> {
        let wire: StatementWire = serde_json::from_slice(bytes)?;
        if wire.statement_type != STATEMENT_TYPE {
            return Err(ParseError::WrongStatementType(wire.statement_type));
        }
        Ok(Self {
            subject: wire.subject.into_iter().map(Subject::from_wire).collect(),
            predicate_type: wire.predicate_type,
            predicate: wire.predicate,
        })
    }
}

// ---------------------------------------------------------------------------
// Unsigned forensic record
// ---------------------------------------------------------------------------

/// An explicitly-unsigned forensic record: a raw in-toto Statement, written
/// when no signing key is configured. A **distinct type**, not a flag — it
/// serializes as the bare statement and structurally cannot grow a
/// `signatures` field, so "unsigned envelope" never appears on a wire.
#[derive(Debug, Clone, PartialEq)]
pub struct UnsignedStatement(Statement);

impl From<Statement> for UnsignedStatement {
    fn from(statement: Statement) -> Self {
        Self(statement)
    }
}

impl UnsignedStatement {
    pub fn statement(&self) -> &Statement {
        &self.0
    }

    /// The forensic record's bytes: exactly the raw statement JSON.
    pub fn to_json_vec(&self) -> Vec<u8> {
        self.0.to_json_vec()
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SignatureEntry {
    /// Unauthenticated hint — carried verbatim, never trusted (see module
    /// docs). Empty when the source envelope had none.
    keyid: String,
    sig: [u8; 64],
}

/// A signed DSSE envelope. Invariants held by construction: the payload type
/// is [`PAYLOAD_TYPE`], the payload and every signature decoded from
/// canonical base64url, signatures are 64 bytes each, and there is at least
/// one — the unsigned form is [`UnsignedStatement`], a different type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Raw (decoded) statement bytes — the bytes the signature covers.
    payload: Vec<u8>,
    signatures: Vec<SignatureEntry>,
}

impl Envelope {
    /// Sign a statement into an envelope.
    ///
    /// The signature is Ed25519 over `PAE(PAYLOAD_TYPE, statement JSON)`,
    /// produced by the substrate signer — this crate never touches key
    /// bytes. The emitted `keyid` is the ADR-012 canonical kid of the
    /// signer's public key.
    pub fn sign(statement: &Statement, signer: &Ed25519RootSigner) -> Self {
        let payload = statement.to_json_vec();
        let sig = signer.sign_message(&pae(PAYLOAD_TYPE, &payload));
        Self {
            payload,
            signatures: vec![SignatureEntry {
                keyid: leyline_sign::kid::canonical_kid(&signer.verifying_key()),
                sig: sig.to_bytes(),
            }],
        }
    }

    /// Compact envelope JSON — the wire form.
    pub fn to_json_vec(&self) -> Vec<u8> {
        let wire = EnvelopeWire {
            payload_type: PAYLOAD_TYPE.to_string(),
            payload: B64.encode(&self.payload),
            signatures: self
                .signatures
                .iter()
                .map(|s| SignatureWire {
                    keyid: s.keyid.clone(),
                    sig: B64.encode(s.sig),
                })
                .collect(),
        };
        // Infallible: strings and vectors only.
        serde_json::to_vec(&wire).expect("envelope serialization cannot fail")
    }

    /// Parse an envelope, enforcing every shape invariant listed on the type.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, ParseError> {
        let wire: EnvelopeWire = serde_json::from_slice(bytes)?;
        if wire.payload_type != PAYLOAD_TYPE {
            return Err(ParseError::WrongPayloadType(wire.payload_type));
        }
        if wire.signatures.is_empty() {
            return Err(ParseError::NoSignatures);
        }
        let payload = B64
            .decode(wire.payload.as_bytes())
            .map_err(ParseError::PayloadEncoding)?;
        let signatures = wire
            .signatures
            .into_iter()
            .map(|s| {
                let sig: [u8; 64] = B64
                    .decode(s.sig.as_bytes())
                    .map_err(|_| ParseError::SignatureEncoding)?
                    .try_into()
                    .map_err(|_| ParseError::SignatureEncoding)?;
                Ok(SignatureEntry {
                    keyid: s.keyid,
                    sig,
                })
            })
            .collect::<Result<Vec<_>, ParseError>>()?;
        Ok(Self {
            payload,
            signatures,
        })
    }

    /// The `keyid` hints, in signature order. Hints only — display and
    /// diagnostics, never trust decisions.
    pub fn keyids(&self) -> Vec<&str> {
        self.signatures.iter().map(|s| s.keyid.as_str()).collect()
    }

    /// Verify every signature under the supplied key, then parse and return
    /// the authenticated statement.
    ///
    /// `keyid` is deliberately ignored: it is an unauthenticated hint, so
    /// trusting it would let an attacker steer key selection. The caller's
    /// key is tried against the actual signature bytes — which is why both
    /// canonical-kid and legacy `hex(sha256(pubkey))` envelopes (and
    /// hint-less ones) verify identically.
    pub fn verify(&self, key: &VerifyingKey) -> Result<Statement, VerifyError> {
        let pae_bytes = pae(PAYLOAD_TYPE, &self.payload);
        for entry in &self.signatures {
            let sig = ed25519_dalek::Signature::from_bytes(&entry.sig);
            if key.verify(&pae_bytes, &sig).is_err() {
                return Err(VerifyError::SignatureInvalid);
            }
        }
        // Only authenticated bytes reach the statement parser.
        Ok(Statement::from_json_slice(&self.payload)?)
    }
}

// ---------------------------------------------------------------------------
// Tests. These live in the lib (not tests/) because the diff-scoped mutants
// gate sees lib tests only.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── rosary byte-compat vector ─────────────────────────────────────
    //
    // Generated by running rosary/src/dsse.rs's reference logic (verbatim
    // replica, ed25519-dalek 2 as pinned by rosary's manifest) over its own
    // `vector_gen` inputs: seed 0102…20, the sample handoff predicate,
    // subject = the pretty-printed on-disk bytes. Every literal below is
    // pinned OUTPUT of that run — nothing here is derived by the code under
    // test.

    const VECTOR_SEED: [u8; 32] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
        26, 27, 28, 29, 30, 31, 32,
    ];
    const VECTOR_PUBKEY_HEX: &str =
        "79b5562e8fe654f94078b112e8a98ba7901f853ae695bed7e0e3910bad049664";
    const VECTOR_STATEMENT_JSON: &str = r#"{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":".rsry-handoff-0.json","digest":{"sha256":"eefc8488445e147de86d582c8f61504ce9ed3971a75c2595b0c2d8ff8c8b30b7"}}],"predicateType":"https://rosary.dev/Handoff/v1","predicate":{"bead_id":"rosary-test","from_agent":"dev-agent","phase":0,"summary":"Fixed the thing."}}"#;
    const VECTOR_PAYLOAD_B64: &str = "eyJfdHlwZSI6Imh0dHBzOi8vaW4tdG90by5pby9TdGF0ZW1lbnQvdjEiLCJzdWJqZWN0IjpbeyJuYW1lIjoiLnJzcnktaGFuZG9mZi0wLmpzb24iLCJkaWdlc3QiOnsic2hhMjU2IjoiZWVmYzg0ODg0NDVlMTQ3ZGU4NmQ1ODJjOGY2MTUwNGNlOWVkMzk3MWE3NWMyNTk1YjBjMmQ4ZmY4YzhiMzBiNyJ9fV0sInByZWRpY2F0ZVR5cGUiOiJodHRwczovL3Jvc2FyeS5kZXYvSGFuZG9mZi92MSIsInByZWRpY2F0ZSI6eyJiZWFkX2lkIjoicm9zYXJ5LXRlc3QiLCJmcm9tX2FnZW50IjoiZGV2LWFnZW50IiwicGhhc2UiOjAsInN1bW1hcnkiOiJGaXhlZCB0aGUgdGhpbmcuIn19";
    const VECTOR_SIG_B64: &str =
        "GyM3MI_KILb9e9O-ySSYa1SWrtFulVlLwigwCLTZJz6P9P8jHHTSj7Ljk3wYzOriHyc-IOqdJ0e9h3oW9M98Cg";
    const VECTOR_CANONICAL_KID: &str = "646d6be49d9f0048f94f67749eca3515";
    /// The envelope rosary's emitter produces today — legacy
    /// `hex(sha256(pubkey))` keyid. Must verify forever.
    const VECTOR_LEGACY_ENVELOPE: &str = r#"{"payloadType":"application/vnd.in-toto+json","payload":"eyJfdHlwZSI6Imh0dHBzOi8vaW4tdG90by5pby9TdGF0ZW1lbnQvdjEiLCJzdWJqZWN0IjpbeyJuYW1lIjoiLnJzcnktaGFuZG9mZi0wLmpzb24iLCJkaWdlc3QiOnsic2hhMjU2IjoiZWVmYzg0ODg0NDVlMTQ3ZGU4NmQ1ODJjOGY2MTUwNGNlOWVkMzk3MWE3NWMyNTk1YjBjMmQ4ZmY4YzhiMzBiNyJ9fV0sInByZWRpY2F0ZVR5cGUiOiJodHRwczovL3Jvc2FyeS5kZXYvSGFuZG9mZi92MSIsInByZWRpY2F0ZSI6eyJiZWFkX2lkIjoicm9zYXJ5LXRlc3QiLCJmcm9tX2FnZW50IjoiZGV2LWFnZW50IiwicGhhc2UiOjAsInN1bW1hcnkiOiJGaXhlZCB0aGUgdGhpbmcuIn19","signatures":[{"keyid":"65b60673d6ed884bf01c2c222d82ada0740f29ac3355d6a925c81f17f47a27b8","sig":"GyM3MI_KILb9e9O-ySSYa1SWrtFulVlLwigwCLTZJz6P9P8jHHTSj7Ljk3wYzOriHyc-IOqdJ0e9h3oW9M98Cg"}]}"#;
    /// The same (statement, key) signed by THIS crate: payload and sig bytes
    /// identical to rosary's; only the unauthenticated keyid hint differs
    /// (canonical kid — the documented migration).
    const VECTOR_CANONICAL_ENVELOPE: &str = r#"{"payloadType":"application/vnd.in-toto+json","payload":"eyJfdHlwZSI6Imh0dHBzOi8vaW4tdG90by5pby9TdGF0ZW1lbnQvdjEiLCJzdWJqZWN0IjpbeyJuYW1lIjoiLnJzcnktaGFuZG9mZi0wLmpzb24iLCJkaWdlc3QiOnsic2hhMjU2IjoiZWVmYzg0ODg0NDVlMTQ3ZGU4NmQ1ODJjOGY2MTUwNGNlOWVkMzk3MWE3NWMyNTk1YjBjMmQ4ZmY4YzhiMzBiNyJ9fV0sInByZWRpY2F0ZVR5cGUiOiJodHRwczovL3Jvc2FyeS5kZXYvSGFuZG9mZi92MSIsInByZWRpY2F0ZSI6eyJiZWFkX2lkIjoicm9zYXJ5LXRlc3QiLCJmcm9tX2FnZW50IjoiZGV2LWFnZW50IiwicGhhc2UiOjAsInN1bW1hcnkiOiJGaXhlZCB0aGUgdGhpbmcuIn19","signatures":[{"keyid":"646d6be49d9f0048f94f67749eca3515","sig":"GyM3MI_KILb9e9O-ySSYa1SWrtFulVlLwigwCLTZJz6P9P8jHHTSj7Ljk3wYzOriHyc-IOqdJ0e9h3oW9M98Cg"}]}"#;

    fn vector_signer() -> Ed25519RootSigner {
        Ed25519RootSigner::from_seed(&VECTOR_SEED)
    }

    fn vector_pubkey() -> VerifyingKey {
        let raw: [u8; 32] = hex::decode(VECTOR_PUBKEY_HEX)
            .expect("hex")
            .try_into()
            .expect("32 bytes");
        VerifyingKey::from_bytes(&raw).expect("valid key")
    }

    /// Rosary's sample handoff, subject-digested over the same
    /// pretty-printed "disk bytes" its vector uses.
    fn vector_statement() -> Statement {
        let predicate = serde_json::json!({
            "phase": 0,
            "from_agent": "dev-agent",
            "bead_id": "rosary-test",
            "summary": "Fixed the thing."
        });
        let disk = serde_json::to_vec_pretty(&predicate).expect("pretty");
        Statement::new(
            vec![Subject::sha256_of(".rsry-handoff-0.json", &disk)],
            "https://rosary.dev/Handoff/v1",
            predicate,
        )
    }

    #[test]
    fn statement_serializes_to_rosary_exact_bytes() {
        assert_eq!(
            vector_statement().to_json_vec(),
            VECTOR_STATEMENT_JSON.as_bytes()
        );
    }

    #[test]
    fn signing_reproduces_rosary_payload_and_signature_bytes() {
        let env = Envelope::sign(&vector_statement(), &vector_signer());
        assert_eq!(env.to_json_vec(), VECTOR_CANONICAL_ENVELOPE.as_bytes());
        // The two rosary-compat fields, asserted against their own pins so a
        // failure localizes to payload encoding vs signing.
        let wire: serde_json::Value = serde_json::from_slice(&env.to_json_vec()).expect("json");
        assert_eq!(wire["payload"], VECTOR_PAYLOAD_B64);
        assert_eq!(wire["signatures"][0]["sig"], VECTOR_SIG_B64);
    }

    #[test]
    fn rosary_legacy_envelope_verifies_under_the_same_key() {
        let env = Envelope::from_json_slice(VECTOR_LEGACY_ENVELOPE.as_bytes()).expect("parse");
        let stmt = env
            .verify(&vector_pubkey())
            .expect("legacy envelopes verify forever");
        assert_eq!(stmt.predicate()["bead_id"], "rosary-test");
        assert_eq!(stmt.predicate_type(), "https://rosary.dev/Handoff/v1");
        assert_eq!(stmt.subject()[0].name(), ".rsry-handoff-0.json");
        assert_eq!(
            stmt.subject()[0].sha256_hex(),
            Some("eefc8488445e147de86d582c8f61504ce9ed3971a75c2595b0c2d8ff8c8b30b7")
        );
    }

    #[test]
    fn parsed_envelope_reserializes_byte_identically() {
        let env = Envelope::from_json_slice(VECTOR_LEGACY_ENVELOPE.as_bytes()).expect("parse");
        assert_eq!(env.to_json_vec(), VECTOR_LEGACY_ENVELOPE.as_bytes());
    }

    // ── keyid: emission vs acceptance ─────────────────────────────────

    #[test]
    fn emitter_writes_the_canonical_kid() {
        let signer = vector_signer();
        let env = Envelope::sign(&vector_statement(), &signer);
        // Against the pinned vector AND the live ADR-012 derivation, so a
        // drift in either direction is caught.
        assert_eq!(env.keyids(), [VECTOR_CANONICAL_KID]);
        assert_eq!(
            env.keyids(),
            [leyline_sign::kid::canonical_kid(&signer.verifying_key()).as_str()]
        );
        assert!(leyline_sign::kid::is_canonical_kid_shape(
            env.keyids()[0].as_bytes()
        ));
    }

    /// keyid is an unauthenticated hint: verification must succeed with no
    /// keyid at all, and with a hint that matches nothing.
    #[test]
    fn verify_never_consults_the_keyid_hint() {
        let signed = Envelope::sign(&vector_statement(), &vector_signer()).to_json_vec();
        let wire: serde_json::Value = serde_json::from_slice(&signed).expect("json");
        let payload = wire["payload"].as_str().expect("payload");
        let sig = wire["signatures"][0]["sig"].as_str().expect("sig");

        let hintless = format!(
            r#"{{"payloadType":"application/vnd.in-toto+json","payload":"{payload}","signatures":[{{"sig":"{sig}"}}]}}"#
        );
        let env = Envelope::from_json_slice(hintless.as_bytes()).expect("parse");
        assert_eq!(env.keyids(), [""]);
        env.verify(&vector_pubkey())
            .expect("hint-less envelope verifies");
        // A parsed hint-less signature also reserializes without a keyid.
        assert_eq!(env.to_json_vec(), hintless.as_bytes());

        let misleading = format!(
            r#"{{"payloadType":"application/vnd.in-toto+json","payload":"{payload}","signatures":[{{"keyid":"totally-wrong-hint","sig":"{sig}"}}]}}"#
        );
        let env = Envelope::from_json_slice(misleading.as_bytes()).expect("parse");
        env.verify(&vector_pubkey())
            .expect("a wrong hint cannot suppress a valid signature");
    }

    // ── round-trip + tamper falsifiers ────────────────────────────────

    fn small_statement() -> Statement {
        Statement::new(
            vec![Subject::sha256_of("s", b"x")],
            "https://example.invalid/T/v1",
            serde_json::json!({"k": "v"}),
        )
    }

    fn envelope_json_with(payload: &[u8], sig: &[u8; 64]) -> Vec<u8> {
        format!(
            r#"{{"payloadType":"application/vnd.in-toto+json","payload":"{}","signatures":[{{"sig":"{}"}}]}}"#,
            B64.encode(payload),
            B64.encode(sig)
        )
        .into_bytes()
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let stmt = small_statement();
        let signer = vector_signer();
        let env = Envelope::sign(&stmt, &signer);
        let back = env.verify(&signer.verifying_key()).expect("verifies");
        assert_eq!(back, stmt);
        // And through the wire.
        let env2 = Envelope::from_json_slice(&env.to_json_vec()).expect("parse");
        assert_eq!(
            env2.verify(&signer.verifying_key()).expect("verifies"),
            stmt
        );
    }

    #[test]
    fn every_flipped_payload_byte_is_refused() {
        let signer = vector_signer();
        let env = Envelope::sign(&small_statement(), &signer);
        for i in 0..env.payload.len() {
            let mut tampered = env.payload.clone();
            tampered[i] ^= 0x01;
            let parsed =
                Envelope::from_json_slice(&envelope_json_with(&tampered, &env.signatures[0].sig))
                    .expect("shape stays valid — only the content changed");
            assert!(
                parsed.verify(&signer.verifying_key()).is_err(),
                "payload byte {i} flipped but verification passed"
            );
        }
    }

    #[test]
    fn every_flipped_signature_byte_is_refused() {
        let signer = vector_signer();
        let env = Envelope::sign(&small_statement(), &signer);
        for i in 0..64 {
            let mut sig = env.signatures[0].sig;
            sig[i] ^= 0x01;
            let parsed = Envelope::from_json_slice(&envelope_json_with(&env.payload, &sig))
                .expect("shape stays valid — only the content changed");
            assert!(
                parsed.verify(&signer.verifying_key()).is_err(),
                "signature byte {i} flipped but verification passed"
            );
        }
    }

    #[test]
    fn wrong_key_is_refused() {
        let env = Envelope::sign(&small_statement(), &vector_signer());
        let other = Ed25519RootSigner::from_seed(&[9u8; 32]);
        assert!(matches!(
            env.verify(&other.verifying_key()),
            Err(VerifyError::SignatureInvalid)
        ));
    }

    // ── PAE ───────────────────────────────────────────────────────────
    //
    // Literal expected bytes, not reconstructed through the function — the
    // goalpost is the DSSE spec's encoding, not our own implementation.

    #[test]
    fn pae_matches_the_spec_literal() {
        assert_eq!(pae("type", b"body"), b"DSSEv1 4 type 4 body");
    }

    #[test]
    fn pae_empty_body_literal() {
        assert_eq!(pae("text/plain", b""), b"DSSEv1 10 text/plain 0 ");
    }

    #[test]
    fn pae_length_is_byte_count_not_char_count() {
        // "café" is 4 chars but 5 UTF-8 bytes.
        assert_eq!(pae("t", "café".as_bytes()), b"DSSEv1 1 t 5 caf\xc3\xa9");
    }

    /// Prefix-injection resistance: for a fixed type, PAE(t, b1) is never a
    /// proper-or-equal prefix of PAE(t, b2) when b1 != b2. Deterministic
    /// corpus: every body of length 0..=3 over an alphabet chosen to include
    /// the framing hazards (space, digit, 'D' of "DSSEv1", NUL) — this covers
    /// the classic attack where b2 = b1 + suffix.
    #[test]
    fn pae_is_prefix_injection_resistant() {
        let alphabet: [u8; 4] = [0x00, b' ', b'1', b'D'];
        let mut bodies: Vec<Vec<u8>> = vec![vec![]];
        for len in 1..=3usize {
            let mut next = Vec::new();
            for body in bodies.iter().filter(|b| b.len() == len - 1) {
                for &c in &alphabet {
                    let mut b = body.clone();
                    b.push(c);
                    next.push(b);
                }
            }
            bodies.extend(next);
        }
        assert_eq!(bodies.len(), 1 + 4 + 16 + 64);
        for b1 in &bodies {
            for b2 in &bodies {
                if b1 == b2 {
                    continue;
                }
                let p1 = pae(PAYLOAD_TYPE, b1);
                let p2 = pae(PAYLOAD_TYPE, b2);
                assert!(
                    !p2.starts_with(&p1),
                    "PAE({b1:?}) is a prefix of PAE({b2:?})"
                );
            }
        }
    }

    // ── unsigned forensic record ──────────────────────────────────────

    #[test]
    fn unsigned_forensic_record_is_the_raw_statement() {
        let stmt = vector_statement();
        let unsigned = UnsignedStatement::from(stmt.clone());
        assert_eq!(unsigned.statement(), &stmt);
        // Bytes are exactly the statement — pinned against the rosary vector.
        assert_eq!(unsigned.to_json_vec(), VECTOR_STATEMENT_JSON.as_bytes());
        // Structurally a statement, never an envelope: correct _type marker,
        // no signatures field anywhere.
        let value: serde_json::Value =
            serde_json::from_slice(&unsigned.to_json_vec()).expect("json");
        assert_eq!(value["_type"], STATEMENT_TYPE);
        assert!(value.get("signatures").is_none());
        assert!(value.get("payload").is_none());
    }

    // ── shape invariants (parse, don't validate) ──────────────────────

    #[test]
    fn envelope_parse_rejects_wrong_payload_type() {
        let json = VECTOR_LEGACY_ENVELOPE.replace(PAYLOAD_TYPE, "application/json");
        assert!(matches!(
            Envelope::from_json_slice(json.as_bytes()),
            Err(ParseError::WrongPayloadType(_))
        ));
    }

    #[test]
    fn envelope_parse_rejects_empty_signatures() {
        // The unsigned form is UnsignedStatement; an empty-signature envelope
        // must be unrepresentable.
        let json = format!(
            r#"{{"payloadType":"{PAYLOAD_TYPE}","payload":"{VECTOR_PAYLOAD_B64}","signatures":[]}}"#
        );
        assert!(matches!(
            Envelope::from_json_slice(json.as_bytes()),
            Err(ParseError::NoSignatures)
        ));
    }

    #[test]
    fn envelope_parse_rejects_non_canonical_payload_encoding() {
        // Padded base64 is valid RFC 4648 but not the canonical no-pad form;
        // accepting it would break parse→serialize byte-identity.
        let json = format!(
            r#"{{"payloadType":"{PAYLOAD_TYPE}","payload":"AA==","signatures":[{{"sig":"{VECTOR_SIG_B64}"}}]}}"#
        );
        assert!(matches!(
            Envelope::from_json_slice(json.as_bytes()),
            Err(ParseError::PayloadEncoding(_))
        ));
    }

    #[test]
    fn envelope_parse_rejects_wrong_length_signature() {
        let short = B64.encode([0u8; 10]);
        let json = format!(
            r#"{{"payloadType":"{PAYLOAD_TYPE}","payload":"{VECTOR_PAYLOAD_B64}","signatures":[{{"sig":"{short}"}}]}}"#
        );
        assert!(matches!(
            Envelope::from_json_slice(json.as_bytes()),
            Err(ParseError::SignatureEncoding)
        ));
    }

    #[test]
    fn envelope_parse_rejects_malformed_json() {
        assert!(matches!(
            Envelope::from_json_slice(b"not json"),
            Err(ParseError::Json(_))
        ));
    }

    #[test]
    fn statement_parse_rejects_wrong_type_marker() {
        let json = VECTOR_STATEMENT_JSON.replace("/Statement/v1", "/Statement/v0.1");
        assert!(matches!(
            Statement::from_json_slice(json.as_bytes()),
            Err(ParseError::WrongStatementType(_))
        ));
    }

    #[test]
    fn statement_parse_round_trips_the_vector() {
        let stmt = Statement::from_json_slice(VECTOR_STATEMENT_JSON.as_bytes()).expect("parse");
        assert_eq!(stmt, vector_statement());
        assert_eq!(stmt.to_json_vec(), VECTOR_STATEMENT_JSON.as_bytes());
    }

    #[test]
    fn subject_digest_is_sha256_of_the_given_bytes() {
        // Independent computation, not through Subject.
        let expected = hex::encode(Sha256::digest(b"artifact bytes"));
        let subject = Subject::sha256_of("a", b"artifact bytes");
        assert_eq!(subject.name(), "a");
        assert_eq!(subject.sha256_hex(), Some(expected.as_str()));
    }
}
