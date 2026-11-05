//! Reading a `Head`'s signed claim and enforcing its signature (bead
//! `ley-line-open-b68fa6`).
//!
//! The signature on `${db}.head.capnp` covers the v2 head digest over
//! [`HeadClaim`]: `generation`, `rootHash`, `parentHash`, `parentDigest`,
//! `treeRoot` and `parserId`. Both readers of a head — the parse that chains
//! onto it ([`crate::cmd_parse`]) and `leyline verify-head`
//! ([`crate::tree_root`]) — read the claim and apply the signature policy
//! through this module, so the two cannot disagree about which fields a valid
//! signature attests.

use std::path::Path;

use anyhow::{Context, Result, bail};
use leyline_core::{Hash, HeadClaim};
use leyline_schema_capnp::head_capnp::head;
use leyline_sign::root_signer::{HeadVerdict, VerifyingKey, verify_head};

/// The head's ADR-0040 stamp: `(treeRoot, parserId)`.
///
/// `Ok(None)` when the head stamps neither (a head written before ADR-0040,
/// or one whose fields were cleared). A head stamps both or neither, so one
/// without the other is malformed and an error (bead `ley-line-open-80f037`
/// F3), as is a hash field that is present but not 32 bytes.
pub fn stamp_of(h: head::Reader<'_>) -> Result<Option<(Hash, Hash)>> {
    let tree = crate::tree_root::hash_field(h.get_tree_root().ok().map(|x| x.get_bytes()))?;
    let parser = crate::tree_root::hash_field(h.get_parser_id().ok().map(|x| x.get_bytes()))?;
    match (tree, parser) {
        (Some(tree), Some(parser)) => Ok(Some((tree, parser))),
        (None, None) => Ok(None),
        (Some(_), None) => bail!("head is half-stamped: it carries treeRoot but no parserId"),
        (None, Some(_)) => bail!("head is half-stamped: it carries parserId but no treeRoot"),
    }
}

/// Read the claim a signature over `h` would attest.
///
/// `Ok(None)` when the head does not stamp `treeRoot` and `parserId` (see
/// [`stamp_of`]): such a head has no v2 claim. A malformed hash field —
/// present but not 32 bytes, or a present all-zero `parentDigest` — and a
/// half-stamped head are errors, never a default.
pub fn claim_of(h: head::Reader<'_>) -> Result<Option<HeadClaim>> {
    let root = required_hash(h.get_root_hash().map(|x| x.get_bytes()), "rootHash")?;
    let parent = required_hash(h.get_parent_hash().map(|x| x.get_bytes()), "parentHash")?;
    let parent_digest = optional_digest(h.get_parent_digest().ok().map(|x| x.get_bytes()))?;
    let Some((tree, parser)) = stamp_of(h)? else {
        return Ok(None);
    };
    Ok(HeadClaim::new(
        h.get_generation(),
        root,
        parent,
        parent_digest,
        tree,
        parser,
    ))
}

/// The v2 digest of `h`'s claim, for the next head's `parentDigest`; `None`
/// when `h` has no claim.
pub fn digest_of(h: head::Reader<'_>) -> Result<Option<Hash>> {
    Ok(claim_of(h)?.as_ref().map(leyline_core::head_digest))
}

/// A hash field every head carries: it must be present and exactly 32 bytes.
/// `parentHash` is all-zero for the first head, which is a value, not
/// absence.
fn required_hash(field: capnp::Result<capnp::Result<&[u8]>>, name: &str) -> Result<Hash> {
    let bytes = field
        .and_then(|b| b)
        .with_context(|| format!("head has no {name}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("head {name} is {} bytes, expected 32", bytes.len()))?;
    Ok(Hash::from_bytes(arr))
}

/// `parentDigest`: unset or empty is absence; anything else must be a
/// 32-byte, non-zero digest. Zero is never read as absence.
fn optional_digest(field: Option<capnp::Result<&[u8]>>) -> Result<Option<Hash>> {
    let Some(field) = field else {
        return Ok(None);
    };
    let bytes = field.context("read head parentDigest")?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("head parentDigest is {} bytes, expected 32", bytes.len()))?;
    if arr == [0u8; 32] {
        bail!("head parentDigest is all zero; absence is an unset field, not zero");
    }
    Ok(Some(Hash::from_bytes(arr)))
}

/// Verdict of `h`'s signature under `trusted`.
pub fn verdict_of(h: head::Reader<'_>, trusted: &[VerifyingKey]) -> Result<HeadVerdict> {
    let claim = claim_of(h)?;
    let signature = h.get_signature().unwrap_or(&[]);
    let signer_kid = h.get_signer_kid().unwrap_or(&[]);
    Ok(verify_head(claim.as_ref(), signature, signer_kid, trusted))
}

/// Which reader applies the policy. The two differ only on an unsigned head
/// read while a trust set is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// A parse chaining onto the previous head. An unsigned head is accepted
    /// unless `LEYLINE_HEAD_REQUIRE_SIGNATURE` is set, so configuring a trust
    /// set does not break an existing unsigned arena's next parse.
    Chain,
    /// `leyline verify-head`, whose output is a claim about the tree. With a
    /// trust set configured an unsigned head is refused: a stripped signature
    /// must not read as valid. With no trust set,
    /// `LEYLINE_HEAD_REQUIRE_SIGNATURE` refuses, because nothing can be
    /// verified.
    VerifyHead,
}

/// What the signature check established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureCheck {
    /// The signature verified under a trusted key.
    Verified,
    /// The head is unsigned and the reader's policy accepted that.
    Unsigned,
    /// No trust set is configured, so the signature was not checked.
    NotChecked,
}

impl std::fmt::Display for SignatureCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SignatureCheck::Verified => "verified",
            SignatureCheck::Unsigned => "unsigned",
            SignatureCheck::NotChecked => "not-checked (no LEYLINE_HEAD_TRUSTED_KEYS)",
        })
    }
}

/// The operator's head trust configuration: the trusted keys
/// (`LEYLINE_HEAD_TRUSTED_KEYS`) and whether an unsigned head is refused
/// (`LEYLINE_HEAD_REQUIRE_SIGNATURE`). Read from the environment once, at
/// the command boundary, and passed down.
#[derive(Clone, Default)]
pub struct HeadPolicy {
    pub trusted: Vec<VerifyingKey>,
    pub require_signature: bool,
}

impl HeadPolicy {
    /// The policy the process environment configures.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            trusted: crate::cmd_parse::head_trusted_keys_from_env()?,
            require_signature: std::env::var("LEYLINE_HEAD_REQUIRE_SIGNATURE")
                .is_ok_and(|v| matches!(v.trim(), "1" | "true")),
        })
    }
}

/// Apply the head signature policy (S2) to `h`, read from `head_path`.
///
/// An invalid signature is always refused. With no trusted keys there is
/// nothing to verify against; see [`Reader`] for how each reader treats an
/// unsigned head.
pub fn enforce_signature(
    h: head::Reader<'_>,
    head_path: &Path,
    reader: Reader,
    policy: &HeadPolicy,
) -> Result<SignatureCheck> {
    let trusted = &policy.trusted;
    if trusted.is_empty() {
        if reader == Reader::VerifyHead && policy.require_signature {
            bail!(
                "LEYLINE_HEAD_REQUIRE_SIGNATURE is set but LEYLINE_HEAD_TRUSTED_KEYS is not, so the signature on the head at {} cannot be verified",
                head_path.display()
            );
        }
        return Ok(SignatureCheck::NotChecked);
    }
    match verdict_of(h, trusted)? {
        HeadVerdict::Valid => Ok(SignatureCheck::Verified),
        // Never acceptable: the head was edited after signing, corrupted,
        // signed by a key this reader does not trust, signed without the
        // treeRoot/parserId the signature must cover, or signed under the
        // removed v1 digest.
        HeadVerdict::Invalid => bail!(
            "head at {} failed signature verification (the signature must cover the whole claim under head digest v2; a head signed before v2 must be re-signed by deleting it and reparsing)",
            head_path.display()
        ),
        HeadVerdict::Unsigned => {
            if reader == Reader::VerifyHead {
                bail!(
                    "head at {} is unsigned and LEYLINE_HEAD_TRUSTED_KEYS is set; verify-head refuses an unsigned head when a trust set is configured",
                    head_path.display()
                );
            }
            if policy.require_signature {
                bail!(
                    "head at {} is unsigned and LEYLINE_HEAD_REQUIRE_SIGNATURE is set",
                    head_path.display()
                );
            }
            Ok(SignatureCheck::Unsigned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(f: impl FnOnce(&mut head::Builder)) -> Vec<u8> {
        let mut msg = capnp::message::Builder::new_default();
        {
            let mut h: head::Builder = msg.init_root();
            h.set_generation(3);
            h.reborrow().init_root_hash().set_bytes(&[1u8; 32]);
            h.reborrow().init_parent_hash().set_bytes(&[0u8; 32]);
            h.reborrow().init_tree_root().set_bytes(&[4u8; 32]);
            h.reborrow().init_parser_id().set_bytes(&[6u8; 32]);
            f(&mut h);
        }
        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &msg).unwrap();
        buf
    }

    fn claim_from(bytes: &[u8]) -> Result<Option<HeadClaim>> {
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        claim_of(msg.get_root().unwrap())
    }

    fn want(parent_digest: Option<Hash>) -> Option<HeadClaim> {
        HeadClaim::new(
            3,
            Hash::from_bytes([1u8; 32]),
            Hash::ZERO,
            parent_digest,
            Hash::from_bytes([4u8; 32]),
            Hash::from_bytes([6u8; 32]),
        )
    }

    #[test]
    fn a_stamped_head_reads_as_its_claim() {
        let got = claim_from(&build(|_| {})).unwrap();
        assert!(got.is_some());
        assert_eq!(got, want(None));
        let with_parent = build(|h| h.reborrow().init_parent_digest().set_bytes(&[8u8; 32]));
        assert_eq!(
            claim_from(&with_parent).unwrap(),
            want(Some(Hash::from_bytes([8u8; 32])))
        );
        let empty_parent = build(|h| h.reborrow().init_parent_digest().set_bytes(&[]));
        assert_eq!(claim_from(&empty_parent).unwrap(), want(None));
    }

    /// `digest_of` is the digest of the claim, and `None` without one.
    #[test]
    fn digest_of_is_the_claim_digest() {
        let bytes = build(|_| {});
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        assert_eq!(
            digest_of(msg.get_root().unwrap()).unwrap(),
            Some(leyline_core::head_digest(&want(None).unwrap()))
        );
        let bytes = build(|h| {
            h.reborrow().init_tree_root().set_bytes(&[]);
            h.reborrow().init_parser_id().set_bytes(&[]);
        });
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        assert_eq!(digest_of(msg.get_root().unwrap()).unwrap(), None);
    }

    /// Unstamped (pre-ADR-0040) heads have no claim; the signature verifier
    /// then refuses any signature on them.
    #[test]
    fn an_unstamped_head_has_no_claim() {
        let unset = build(|h| {
            h.reborrow().init_tree_root().set_bytes(&[]);
            h.reborrow().init_parser_id().set_bytes(&[]);
        });
        assert_eq!(claim_from(&unset).unwrap(), None);
        let zero = build(|h| {
            h.reborrow().init_tree_root().set_bytes(&[0u8; 32]);
            h.reborrow().init_parser_id().set_bytes(&[0u8; 32]);
        });
        assert_eq!(claim_from(&zero).unwrap(), None);
    }

    /// Bead `ley-line-open-80f037` F3(d): a head stamps both `treeRoot` and
    /// `parserId` or neither. One without the other is a malformed head, not
    /// one written before ADR-0040.
    #[test]
    fn a_half_stamped_head_is_malformed() {
        for half in [
            build(|h| h.reborrow().init_tree_root().set_bytes(&[])),
            build(|h| h.reborrow().init_parser_id().set_bytes(&[0u8; 32])),
        ] {
            let err = claim_from(&half)
                .expect_err("half-stamped head")
                .to_string();
            assert!(err.contains("half-stamped"), "{err}");
        }
    }

    /// A malformed hash is an error, not a silent ZERO.
    #[test]
    fn a_malformed_hash_field_is_an_error() {
        let short_parent = build(|h| h.reborrow().init_parent_hash().set_bytes(&[0u8; 5]));
        assert!(claim_from(&short_parent).is_err());
        let short_root = build(|h| h.reborrow().init_root_hash().set_bytes(&[1u8; 31]));
        assert!(claim_from(&short_root).is_err());
        let short_tree = build(|h| h.reborrow().init_tree_root().set_bytes(&[4u8; 3]));
        assert!(claim_from(&short_tree).is_err());
        let short_digest = build(|h| h.reborrow().init_parent_digest().set_bytes(&[8u8; 9]));
        assert!(claim_from(&short_digest).is_err());
        let zero_digest = build(|h| h.reborrow().init_parent_digest().set_bytes(&[0u8; 32]));
        let err = claim_from(&zero_digest).unwrap_err().to_string();
        assert!(err.contains("all zero"), "{err}");
    }

    /// A head with no rootHash at all is an error.
    #[test]
    fn a_head_without_a_root_hash_is_an_error() {
        let mut msg = capnp::message::Builder::new_default();
        {
            let mut h: head::Builder = msg.init_root();
            h.set_generation(1);
        }
        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &msg).unwrap();
        let err = claim_from(&buf).unwrap_err().to_string();
        assert!(err.contains("rootHash"), "{err}");
    }

    fn signer() -> leyline_sign::root_signer::Ed25519RootSigner {
        leyline_sign::root_signer::Ed25519RootSigner::from_seed(&[5u8; 32])
    }

    /// The test head, signed over its claim when `signed`.
    fn head_bytes(signed: bool) -> Vec<u8> {
        build(|h| {
            if signed {
                use leyline_core::RootSigner;
                let digest = leyline_core::head_digest(&want(None).unwrap());
                let sig = signer().sign(digest).unwrap().to_bytes();
                h.set_signature(&sig);
            }
        })
    }

    fn enforce(bytes: &[u8], reader: Reader, policy: &HeadPolicy) -> Result<SignatureCheck> {
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        enforce_signature(
            msg.get_root().unwrap(),
            Path::new("t.head.capnp"),
            reader,
            policy,
        )
    }

    fn trusting(require_signature: bool) -> HeadPolicy {
        HeadPolicy {
            trusted: vec![signer().verifying_key()],
            require_signature,
        }
    }

    /// The policy matrix, per reader (identity review C1: with a trust set,
    /// verify-head refuses an unsigned head whatever REQUIRE says).
    #[test]
    fn the_signature_policy_per_reader() {
        let unsigned = head_bytes(false);
        let signed = head_bytes(true);
        let none = HeadPolicy::default();
        let require_only = HeadPolicy {
            trusted: Vec::new(),
            require_signature: true,
        };
        use SignatureCheck::*;

        // No trust set: nothing is checked, except verify-head with REQUIRE.
        assert_eq!(
            enforce(&unsigned, Reader::Chain, &none).unwrap(),
            NotChecked
        );
        assert_eq!(enforce(&signed, Reader::Chain, &none).unwrap(), NotChecked);
        assert_eq!(
            enforce(&unsigned, Reader::Chain, &require_only).unwrap(),
            NotChecked
        );
        assert_eq!(
            enforce(&unsigned, Reader::VerifyHead, &none).unwrap(),
            NotChecked
        );
        assert!(enforce(&signed, Reader::VerifyHead, &require_only).is_err());

        // Trust set, valid signature: verified for both readers.
        for reader in [Reader::Chain, Reader::VerifyHead] {
            assert_eq!(
                enforce(&signed, reader, &trusting(false)).unwrap(),
                Verified
            );
            assert_eq!(enforce(&signed, reader, &trusting(true)).unwrap(), Verified);
        }

        // Trust set, unsigned: the chain accepts unless REQUIRE; verify-head
        // refuses either way.
        assert_eq!(
            enforce(&unsigned, Reader::Chain, &trusting(false)).unwrap(),
            Unsigned
        );
        assert!(enforce(&unsigned, Reader::Chain, &trusting(true)).is_err());
        let err = enforce(&unsigned, Reader::VerifyHead, &trusting(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsigned"), "{err}");
        assert!(enforce(&unsigned, Reader::VerifyHead, &trusting(true)).is_err());

        // Trust set, tampered: refused for both readers.
        let mut tampered = signed.clone();
        let msg = capnp::serialize::read_message(&mut &signed[..], Default::default()).unwrap();
        let old: head::Reader = msg.get_root().unwrap();
        let sig = old.get_signature().unwrap().to_vec();
        tampered.clear();
        tampered.extend(build(|h| {
            h.set_signature(&sig);
            h.reborrow().init_tree_root().set_bytes(&[0x33; 32]);
        }));
        for reader in [Reader::Chain, Reader::VerifyHead] {
            let err = enforce(&tampered, reader, &trusting(false))
                .unwrap_err()
                .to_string();
            assert!(err.contains("failed signature verification"), "{err}");
        }
    }

    #[test]
    fn signature_check_display_names_the_verdict() {
        assert_eq!(SignatureCheck::Verified.to_string(), "verified");
        assert_eq!(SignatureCheck::Unsigned.to_string(), "unsigned");
        assert!(
            SignatureCheck::NotChecked
                .to_string()
                .starts_with("not-checked")
        );
    }

    /// Every `Head` field takes a declared position on the signature: bound
    /// into the v2 digest, or deliberately outside it. A field added to
    /// `head.capnp` fails this test until it is classified here (and, when
    /// bound, until `HeadClaim` and `head_digest` carry it).
    #[test]
    fn every_head_field_is_classified_for_the_signature() {
        // Bound by `head_digest` through `HeadClaim`.
        const BOUND: &[&str] = &[
            "rootHash",
            "parentHash",
            "parentDigest",
            "generation",
            "treeRoot",
            "parserId",
        ];
        // Outside the signature, with the reason:
        // - signature: the signature itself.
        // - signerKid: selects a key and confers no authority (ADR-012 R1).
        // - segmentBytes: a torn-write sanity count over the segments
        //   rootHash already commits to.
        // - unboundFacts: a producer-side ratchet metric, not an identity.
        const UNSIGNED: &[&str] = &["signature", "signerKid", "segmentBytes", "unboundFacts"];

        let mut msg = capnp::message::Builder::new_default();
        let h: head::Builder = msg.init_root();
        let dynamic: capnp::dynamic_value::Reader = h.into_reader().into();
        let mut names: Vec<String> = dynamic
            .downcast::<capnp::dynamic_struct::Reader>()
            .get_schema()
            .get_fields()
            .unwrap()
            .iter()
            .map(|f| f.get_proto().get_name().unwrap().to_string().unwrap())
            .collect();
        names.sort();
        let mut classified: Vec<String> = BOUND
            .iter()
            .chain(UNSIGNED)
            .map(|s| (*s).to_string())
            .collect();
        classified.sort();
        assert_eq!(
            names, classified,
            "a Head field is unclassified for the signature (bead b68fa6)"
        );
    }
}
