//! Ed25519 cert-chain verification + claims parsing.
//!
//! Used by consumers like cloister's lease middleware (cloister-bd7770)
//! to verify that an ephemeral Signet cert was signed by the cluster's
//! master CA, and to extract the cert's claims (validity window,
//! ephemeral pubkey, and the Interlace-specific extensions:
//! peer_fingerprint, scope, epoch).
//!
//! The verifier is generic over input cert bytes — it doesn't know
//! whether the cert was minted by notme's `mintBridgeCertPair` or by
//! some other Signet-aware CA. It only checks that the cert's signature
//! verifies under the supplied master public key. Cert minting is the
//! CA's job (e.g. notme-bd2a72); cert verification is this module's.
//!
//! # Cert format expectations
//!
//! - X.509 v3 certificate with Ed25519 subject + Ed25519 signature.
//! - SubjectPublicKeyInfo carries the ephemeral pubkey (32 bytes).
//! - Validity period as standard X.509 `Time` (UTCTime or GeneralizedTime).
//! - Optional Interlace extensions under the OID arc `1.3.6.1.4.1.99999.*`:
//!     - `1.3.6.1.4.1.99999.1.4` — interlace-epoch, encoded as DER INTEGER.
//!     - `1.3.6.1.4.1.99999.1.5` — interlace-peer, encoded as DER UTF8String.
//!     - `1.3.6.1.4.1.99999.1.6` — interlace-scope, encoded as DER UTF8String.
//!     - `1.3.6.1.4.1.99999.1.7` — confinementDigest, encoded as DER
//!       OctetString containing exactly 32 bytes (BLAKE3-256 of the
//!       cloister/confinement/v1 §6-canonical `ConfinementManifest`).
//!       Lane-2 identity commit for confinement/v1 §7: a substrate
//!       runner enforcing a `ConfinementManifest` whose digest differs
//!       from this claim MUST refuse to start the bundle. Bead
//!       `ley-line-open-c79ea8`.
//!
//! These are documented at notme/worker/src/cert-authority.ts:57-63 (the
//! custom-OID arc is shared with the existing GHA bridge cert minting).
//!
//! Missing Interlace extensions are not an error — the verifier returns
//! the validity window + ephemeral pubkey as required claims, and the
//! extension fields as `Option`. The lease middleware's caller decides
//! whether a cert without an `epoch` claim is acceptable.

use der::{Decode, Encode};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use x509_cert::Certificate;

use crate::oid::ID_ED25519;

/// Result of a successful cert-chain verification + claims extraction.
#[derive(Debug, Clone)]
pub struct CertClaims {
    /// Ephemeral pubkey from SubjectPublicKeyInfo (32 bytes for Ed25519).
    pub ephemeral_pubkey: [u8; 32],
    /// Cert validity window — Unix seconds.
    pub not_before: i64,
    pub not_after: i64,
    /// Interlace-specific custom-OID extensions; None if cert wasn't minted
    /// with these (e.g. legacy cert from a pre-reshape `mintBridgeCertPair`).
    pub epoch: Option<u32>,
    pub peer_fp: Option<String>,
    pub scope: Option<String>,
    /// Bead `ley-line-open-c79ea8`: BLAKE3-256 of the §6-canonical
    /// `ConfinementManifest` (cloister/confinement/v1). `None` when
    /// the cert was minted before this extension existed, or when
    /// the workload doesn't commit to a confinement manifest. When
    /// `Some`, a substrate runner MUST verify at bundle-start that
    /// the manifest it's about to enforce hashes to this digest —
    /// mismatch is a fail-closed identity check.
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
}

/// OID arc for cloister's Interlace cert extensions. Shared with notme's
/// custom-OID extensions (cert-authority.ts:57-63).
mod oid_interlace {
    use const_oid::ObjectIdentifier;
    /// interlace-epoch — DER INTEGER.
    pub const EPOCH: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.4");
    /// interlace-peer — DER UTF8String.
    pub const PEER_FP: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.5");
    /// interlace-scope — DER UTF8String.
    pub const SCOPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.6");
    /// confinementDigest — DER OctetString containing exactly 32 bytes
    /// (BLAKE3-256 of the §6-canonical confinement/v1 manifest). Bead
    /// `ley-line-open-c79ea8`.
    pub const CONFINEMENT_DIGEST: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.1.7");
}

/// Verify the cert is signed by `master_pubkey` and extract its claims.
///
/// On success: returns `CertClaims` with the validity window, ephemeral
/// pubkey, and (optionally) Interlace extensions.
///
/// On failure: returns a `ChainError` describing what went wrong. No
/// info leaks back to the caller beyond the variant — callers that want
/// to log details can match on the variant; production error responses
/// should treat all failures as opaque "auth failed" to avoid signaling
/// to attackers what specifically rejected.
pub fn verify_cert_chain(cert_der: &[u8], master_pubkey: &[u8]) -> Result<CertClaims, ChainError> {
    if master_pubkey.len() != 32 {
        return Err(ChainError::BadMasterKey);
    }

    let cert = Certificate::from_der(cert_der).map_err(|e| ChainError::BadDer(e.to_string()))?;

    // Check signature algorithm is Ed25519.
    if cert.signature_algorithm.oid != ID_ED25519 {
        return Err(ChainError::NotEd25519);
    }

    // Re-encode tbs_certificate as DER — that's what the master signed.
    // We can't just slice the input because the signature covers the
    // canonical DER encoding; trusting the parser to round-trip the
    // exact same bytes is fragile but necessary (no way to access the
    // original TBS slice after parsing).
    let tbs_der = cert
        .tbs_certificate
        .to_der()
        .map_err(|e| ChainError::EncodingFailed(e.to_string()))?;

    let sig_bytes = cert.signature.raw_bytes();
    if sig_bytes.len() != 64 {
        return Err(ChainError::BadSignature);
    }
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| ChainError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_arr);

    let master_arr: [u8; 32] = master_pubkey
        .try_into()
        .map_err(|_| ChainError::BadMasterKey)?;
    let verifying_key =
        VerifyingKey::from_bytes(&master_arr).map_err(|_| ChainError::BadMasterKey)?;

    verifying_key
        .verify(&tbs_der, &signature)
        .map_err(|_| ChainError::BadSignature)?;

    // Signature verified. Now extract claims.

    // Ephemeral pubkey from SPKI.
    let spki = &cert.tbs_certificate.subject_public_key_info;
    if spki.algorithm.oid != ID_ED25519 {
        return Err(ChainError::BadSpki);
    }
    let ephemeral_raw = spki.subject_public_key.raw_bytes();
    if ephemeral_raw.len() != 32 {
        return Err(ChainError::BadSpki);
    }
    let ephemeral_pubkey: [u8; 32] = ephemeral_raw.try_into().map_err(|_| ChainError::BadSpki)?;

    // Validity window — convert Time to Unix seconds.
    let not_before = time_to_unix(&cert.tbs_certificate.validity.not_before);
    let not_after = time_to_unix(&cert.tbs_certificate.validity.not_after);

    // Interlace extensions (optional).
    let (epoch, peer_fp, scope, confinement_digest) = extract_interlace_extensions(&cert)?;

    Ok(CertClaims {
        ephemeral_pubkey,
        not_before,
        not_after,
        epoch,
        peer_fp,
        scope,
        confinement_digest,
    })
}

/// Convert an x509-cert `Time` to Unix-seconds (i64). Both UTCTime and
/// GeneralizedTime variants supported.
fn time_to_unix(t: &x509_cert::time::Time) -> i64 {
    use x509_cert::time::Time;
    match t {
        Time::UtcTime(ut) => ut.to_unix_duration().as_secs() as i64,
        Time::GeneralTime(gt) => gt.to_unix_duration().as_secs() as i64,
    }
}

/// Interlace extension fields extracted from a cert's optional
/// custom-OID extensions. All four are `Option` because the verifier
/// returns the validity window + ephemeral pubkey as required claims
/// even when none of the Interlace extensions are present.
type InterlaceExtensions = (
    Option<u32>,
    Option<String>,
    Option<String>,
    Option<[u8; 32]>,
);

/// Walk cert.tbs_certificate.extensions; pull the four Interlace OIDs
/// if present. Order-independent. Each extension's value is DER-decoded
/// per its expected type.
fn extract_interlace_extensions(cert: &Certificate) -> Result<InterlaceExtensions, ChainError> {
    let mut epoch: Option<u32> = None;
    let mut peer_fp: Option<String> = None;
    let mut scope: Option<String> = None;
    let mut confinement_digest: Option<[u8; 32]> = None;

    let Some(extensions) = &cert.tbs_certificate.extensions else {
        return Ok((None, None, None, None));
    };

    for ext in extensions {
        let value_bytes = ext.extn_value.as_bytes();
        if ext.extn_id == oid_interlace::EPOCH {
            // DER INTEGER → u32.
            let int = der::asn1::Int::from_der(value_bytes)
                .map_err(|_| ChainError::BadExtension("epoch is not a DER INTEGER"))?;
            // x509's Int can be arbitrary-width; coerce to i64 then to u32.
            let raw = int.as_bytes();
            // Big-endian, two's complement. We only support non-negative,
            // ≤ 4 bytes.
            if raw.is_empty() || raw.len() > 5 {
                return Err(ChainError::BadExtension("epoch out of range"));
            }
            // Strip leading 0x00 sign byte if present.
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
            epoch = Some(u32::from_be_bytes(buf));
        } else if ext.extn_id == oid_interlace::PEER_FP {
            let s = der::asn1::Utf8StringRef::from_der(value_bytes)
                .map_err(|_| ChainError::BadExtension("peer_fp is not DER UTF8String"))?;
            peer_fp = Some(s.as_str().to_string());
        } else if ext.extn_id == oid_interlace::SCOPE {
            let s = der::asn1::Utf8StringRef::from_der(value_bytes)
                .map_err(|_| ChainError::BadExtension("scope is not DER UTF8String"))?;
            scope = Some(s.as_str().to_string());
        } else if ext.extn_id == oid_interlace::CONFINEMENT_DIGEST {
            // Bead `ley-line-open-c79ea8` / cloister/confinement/v1 §7:
            // extension value is a DER OctetString wrapping exactly 32
            // bytes (BLAKE3-256 of the §6-canonical manifest). Any other
            // length is a spec violation — reject rather than truncate
            // or pad so a mis-encoded cert can't silently satisfy
            // identity-commit checks downstream.
            let octets = der::asn1::OctetStringRef::from_der(value_bytes).map_err(|_| {
                ChainError::BadExtension("confinementDigest is not DER OctetString")
            })?;
            let bytes = octets.as_bytes();
            if bytes.len() != 32 {
                return Err(ChainError::BadExtension(
                    "confinementDigest must be exactly 32 bytes (BLAKE3-256)",
                ));
            }
            let mut digest = [0u8; 32];
            digest.copy_from_slice(bytes);
            confinement_digest = Some(digest);
        } else if ext.critical {
            // Per RFC 5280 §4.2 + threat-model §6.1.6 (cloister-c71977):
            // a verifier MUST reject any cert it does not recognize when
            // the extension is critical-flagged. Standard X.509 critical
            // extensions (BasicConstraints, KeyUsage, ExtKeyUsage, SAN,
            // etc.) are NOT in the cloister verifier's known set because
            // Interlace certs minted by notme don't carry them today; if
            // they do in the future, expand the allow-list before flipping
            // them critical.
            return Err(ChainError::UnknownCriticalExtension(
                ext.extn_id.to_string(),
            ));
        }
        // Non-critical unknown extensions are ignored per RFC 5280.
    }

    Ok((epoch, peer_fp, scope, confinement_digest))
}

/// Hand-rolled JSON encoding of CertClaims. Used by the FFI export in
/// ffi.rs to write claims into a caller-allocated output buffer.
///
/// Format is stable + minimal:
///   {"epk":"<base64>","nb":1234,"na":5678,"ep":7,"pf":"...","sc":"...","cd":"<base64>"}
///
/// Optional fields (`ep`, `pf`, `sc`, `cd`) are emitted only when present.
/// Strings are escaped per RFC 8259 §7. Caller writes consumed bytes and
/// can parse with `JSON.parse` on the JS side. `cd` is base64url-nopad of
/// the 32-byte confinementDigest (bead `ley-line-open-c79ea8`).
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

/// Test helpers — minting fixtures for cert-chain verification. Public so
/// `examples/gen-fixture.rs` can produce stable test fixtures for the
/// TS-side wasm wrapper. Don't use in production code paths; the helpers
/// are intentionally permissive (no input validation, panics on bad
/// inputs) for ergonomic test code.
pub mod tests_helpers {
    use super::oid_interlace;
    use crate::oid::ID_ED25519;
    use ed25519_dalek::{Signer, SigningKey};

    /// Hand-encode unsigned u32 as a canonical DER INTEGER. `Int::new`
    /// accepts the raw 4-byte BE form but emits leading zeros that
    /// strict parsers reject as non-canonical.
    fn encode_u32_as_der_int(v: u32) -> Vec<u8> {
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

    /// Mint a self-signed-by-master ephemeral cert with the given key
    /// material + Interlace extensions. Returns DER bytes. Test-quality
    /// only — issuer/subject are empty Names, no KeyUsage / EKU, etc.
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
        use der::{
            Encode,
            asn1::{BitString, OctetString, Utf8StringRef},
        };
        use x509_cert::{
            Certificate,
            certificate::TbsCertificate,
            ext::{Extension, Extensions},
            name::{Name, RdnSequence},
            serial_number::SerialNumber,
            spki::{AlgorithmIdentifier, SubjectPublicKeyInfo},
            time::{Time, Validity},
        };

        let alg_ed25519 = AlgorithmIdentifier {
            oid: ID_ED25519,
            parameters: None,
        };

        let spki = SubjectPublicKeyInfo {
            algorithm: alg_ed25519.clone(),
            subject_public_key: BitString::from_bytes(ephemeral.verifying_key().as_bytes())
                .unwrap(),
        };

        let issuer: Name = RdnSequence::default();
        let subject = issuer.clone();

        let validity = Validity {
            not_before: Time::UtcTime(
                der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(
                    not_before as u64,
                ))
                .unwrap(),
            ),
            not_after: Time::UtcTime(
                der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(
                    not_after as u64,
                ))
                .unwrap(),
            ),
        };

        let mut extensions: Vec<Extension> = Vec::new();
        if let Some(ep) = epoch {
            let int_der = encode_u32_as_der_int(ep);
            extensions.push(Extension {
                extn_id: oid_interlace::EPOCH,
                critical: false,
                extn_value: OctetString::new(int_der).unwrap(),
            });
        }
        if let Some(pf) = peer_fp {
            let s_der = Utf8StringRef::new(pf).unwrap().to_der().unwrap();
            extensions.push(Extension {
                extn_id: oid_interlace::PEER_FP,
                critical: false,
                extn_value: OctetString::new(s_der).unwrap(),
            });
        }
        if let Some(sc) = scope {
            let s_der = Utf8StringRef::new(sc).unwrap().to_der().unwrap();
            extensions.push(Extension {
                extn_id: oid_interlace::SCOPE,
                critical: false,
                extn_value: OctetString::new(s_der).unwrap(),
            });
        }
        if let Some(cd) = confinement_digest {
            // Wrap the 32-byte digest in a DER OctetString so the outer
            // extn_value carries `OCTET STRING { OCTET STRING { .. } }`,
            // matching the epoch/peer_fp/scope shape and the §7 spec.
            let inner_der = OctetString::new(cd.to_vec()).unwrap().to_der().unwrap();
            extensions.push(Extension {
                extn_id: oid_interlace::CONFINEMENT_DIGEST,
                critical: false,
                extn_value: OctetString::new(inner_der).unwrap(),
            });
        }

        let tbs = TbsCertificate {
            version: x509_cert::Version::V3,
            serial_number: SerialNumber::new(&[1]).unwrap(),
            signature: alg_ed25519.clone(),
            issuer,
            validity,
            subject,
            subject_public_key_info: spki,
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(Extensions::try_from(extensions).unwrap())
            },
        };

        let tbs_der = tbs.to_der().unwrap();
        let sig = master.sign(&tbs_der);

        let cert = Certificate {
            tbs_certificate: tbs,
            signature_algorithm: alg_ed25519,
            signature: BitString::from_bytes(&sig.to_bytes()).unwrap(),
        };

        cert.to_der().unwrap()
    }

    /// Mint a self-signed-by-master cert with one extra extension on top
    /// of the standard Interlace ones. Used to test cloister-c71977's
    /// rejection of critical-flagged unknown extensions (RFC 5280 §4.2).
    /// Caller picks the OID, the critical flag, and the raw DER value.
    pub fn mint_test_cert_with_extra_ext(
        master: &SigningKey,
        ephemeral: &SigningKey,
        not_before: i64,
        not_after: i64,
        extra_oid: const_oid::ObjectIdentifier,
        extra_critical: bool,
        extra_value_der: Vec<u8>,
    ) -> Vec<u8> {
        use der::{
            Encode,
            asn1::{BitString, OctetString, Utf8StringRef},
        };
        use x509_cert::{
            Certificate,
            certificate::TbsCertificate,
            ext::{Extension, Extensions},
            name::{Name, RdnSequence},
            serial_number::SerialNumber,
            spki::{AlgorithmIdentifier, SubjectPublicKeyInfo},
            time::{Time, Validity},
        };

        let alg_ed25519 = AlgorithmIdentifier {
            oid: ID_ED25519,
            parameters: None,
        };
        let spki = SubjectPublicKeyInfo {
            algorithm: alg_ed25519.clone(),
            subject_public_key: BitString::from_bytes(ephemeral.verifying_key().as_bytes())
                .unwrap(),
        };
        let issuer: Name = RdnSequence::default();
        let subject = issuer.clone();
        let validity = Validity {
            not_before: Time::UtcTime(
                der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(
                    not_before as u64,
                ))
                .unwrap(),
            ),
            not_after: Time::UtcTime(
                der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(
                    not_after as u64,
                ))
                .unwrap(),
            ),
        };

        // Standard Interlace extensions (non-critical) + the extra one.
        let mut extensions: Vec<Extension> = Vec::new();
        let int_der = encode_u32_as_der_int(7);
        extensions.push(Extension {
            extn_id: oid_interlace::EPOCH,
            critical: false,
            extn_value: OctetString::new(int_der).unwrap(),
        });
        let s_der = Utf8StringRef::new("sha256:test").unwrap().to_der().unwrap();
        extensions.push(Extension {
            extn_id: oid_interlace::PEER_FP,
            critical: false,
            extn_value: OctetString::new(s_der).unwrap(),
        });
        let s_der = Utf8StringRef::new("test:scope").unwrap().to_der().unwrap();
        extensions.push(Extension {
            extn_id: oid_interlace::SCOPE,
            critical: false,
            extn_value: OctetString::new(s_der).unwrap(),
        });
        extensions.push(Extension {
            extn_id: extra_oid,
            critical: extra_critical,
            extn_value: OctetString::new(extra_value_der).unwrap(),
        });

        let tbs = TbsCertificate {
            version: x509_cert::Version::V3,
            serial_number: SerialNumber::new(&[1]).unwrap(),
            signature: alg_ed25519.clone(),
            issuer,
            validity,
            subject,
            subject_public_key_info: spki,
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: Some(Extensions::try_from(extensions).unwrap()),
        };

        let tbs_der = tbs.to_der().unwrap();
        let sig = master.sign(&tbs_der);
        let cert = Certificate {
            tbs_certificate: tbs,
            signature_algorithm: alg_ed25519,
            signature: BitString::from_bytes(&sig.to_bytes()).unwrap(),
        };
        cert.to_der().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::RngCore;
    use rand::rngs::OsRng;

    use super::tests_helpers::mint_test_cert;

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

    #[test]
    fn claims_json_minimum_cert() {
        let claims = CertClaims {
            ephemeral_pubkey: [0xAB; 32],
            not_before: 1700000000,
            not_after: 1700000300,
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
    }

    #[test]
    fn claims_json_with_extensions() {
        let claims = CertClaims {
            ephemeral_pubkey: [0xAB; 32],
            not_before: 1,
            not_after: 2,
            epoch: Some(7),
            peer_fp: Some("sha256:abc".to_string()),
            scope: Some("bead_create:*".to_string()),
            confinement_digest: None,
        };
        let j = claims_to_json(&claims);
        assert!(j.contains("\"ep\":7"));
        assert!(j.contains("\"pf\":\"sha256:abc\""));
        assert!(j.contains("\"sc\":\"bead_create:*\""));
    }

    #[test]
    fn claims_json_escapes_strings() {
        let claims = CertClaims {
            ephemeral_pubkey: [0; 32],
            not_before: 0,
            not_after: 0,
            epoch: None,
            peer_fp: Some("a\"b\\c\n".to_string()),
            scope: None,
            confinement_digest: None,
        };
        let j = claims_to_json(&claims);
        // Embedded quote, backslash, newline must be escaped.
        assert!(j.contains("\"pf\":\"a\\\"b\\\\c\\n\""));
    }

    // ── Critical-extension rejection (cloister-c71977 / RFC 5280 §4.2) ──

    use super::tests_helpers::mint_test_cert_with_extra_ext;
    use const_oid::ObjectIdentifier;

    /// An OID under a private arc that the verifier doesn't know about.
    /// Per RFC 5280: when this is critical, the verifier MUST reject.
    const UNKNOWN_PRIVATE_OID: ObjectIdentifier =
        ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.42.1");

    #[test]
    fn rejects_critical_unknown_extension() {
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let nb = now() - 60;
        let na = now() + 600;

        // Mint with a critical-flagged unknown extension.
        let cert = mint_test_cert_with_extra_ext(
            &master,
            &ephemeral,
            nb,
            na,
            UNKNOWN_PRIVATE_OID,
            true,                   // critical
            vec![0x04, 0x01, 0x01], // arbitrary DER (OCTET STRING { 0x01 })
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
        let nb = now() - 60;
        let na = now() + 600;

        // Same OID, but non-critical — RFC 5280 says "MAY ignore".
        let cert = mint_test_cert_with_extra_ext(
            &master,
            &ephemeral,
            nb,
            na,
            UNKNOWN_PRIVATE_OID,
            false, // non-critical
            vec![0x04, 0x01, 0x01],
        );

        let result = verify_cert_chain(&cert, master.verifying_key().as_bytes());
        let claims = result.expect("non-critical unknown should be ignored");
        assert_eq!(claims.epoch, Some(7));
        assert_eq!(claims.peer_fp.as_deref(), Some("sha256:test"));
    }

    #[test]
    fn accepts_critical_known_interlace_extensions() {
        // Sanity: the verifier doesn't reject Interlace extensions even
        // if a future minter flags them critical. Our known-OID list is
        // the EPOCH/PEER_FP/SCOPE arc; critical flag on those is fine.
        let master = random_signing_key();
        let ephemeral = random_signing_key();
        let nb = now() - 60;
        let na = now() + 600;

        // Use the standard mint_test_cert (non-critical) — sanity baseline.
        let cert = mint_test_cert(
            &master,
            &ephemeral,
            nb,
            na,
            Some(7),
            Some("sha256:abc"),
            Some("bead_create:/r/foo"),
            None,
        );

        let claims = verify_cert_chain(&cert, master.verifying_key().as_bytes())
            .expect("standard interlace cert should verify");
        assert_eq!(claims.epoch, Some(7));
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
