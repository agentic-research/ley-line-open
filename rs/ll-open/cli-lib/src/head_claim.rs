//! Reading a `Head`'s signed claim and enforcing its signature (bead
//! `ley-line-open-b68fa6`).
//!
//! The signature on `${db}.head.capnp` covers the v2 head digest over
//! [`HeadClaim`]: `generation`, `rootHash`, `parentHash`, `treeRoot` and
//! `parserId`. Both readers of a head — the parse that chains onto it
//! ([`crate::cmd_parse`]) and `leyline verify-head` ([`crate::tree_root`]) —
//! read the claim and apply the signature policy through this module, so the
//! two cannot disagree about which fields a valid signature attests.

use std::path::Path;

use anyhow::{Context, Result, bail};
use leyline_core::{Hash, HeadClaim};
use leyline_schema_capnp::head_capnp::head;
use leyline_sign::root_signer::{HeadVerdict, VerifyingKey, verify_head};

/// Read the claim a signature over `h` would attest.
///
/// `Ok(None)` when the head does not stamp `treeRoot` and `parserId` (a head
/// written before ADR-0040, or one whose fields were cleared): such a head
/// has no v2 claim. A malformed hash field — present but not 32 bytes — is an
/// error, never a default.
pub fn claim_of(h: head::Reader<'_>) -> Result<Option<HeadClaim>> {
    let root = required_hash(h.get_root_hash().map(|x| x.get_bytes()), "rootHash")?;
    let parent = required_hash(h.get_parent_hash().map(|x| x.get_bytes()), "parentHash")?;
    let tree = crate::tree_root::hash_field(h.get_tree_root().ok().map(|x| x.get_bytes()))?;
    let parser = crate::tree_root::hash_field(h.get_parser_id().ok().map(|x| x.get_bytes()))?;
    let (Some(tree), Some(parser)) = (tree, parser) else {
        return Ok(None);
    };
    Ok(HeadClaim::new(
        h.get_generation(),
        root,
        parent,
        tree,
        parser,
    ))
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

/// Verdict of `h`'s signature under `trusted`.
pub fn verdict_of(h: head::Reader<'_>, trusted: &[VerifyingKey]) -> Result<HeadVerdict> {
    let claim = claim_of(h)?;
    let signature = h.get_signature().unwrap_or(&[]);
    let signer_kid = h.get_signer_kid().unwrap_or(&[]);
    Ok(verify_head(claim.as_ref(), signature, signer_kid, trusted))
}

/// Apply the head signature policy (S2) to `h`, read from `head_path`.
///
/// Runs only when the operator configured `LEYLINE_HEAD_TRUSTED_KEYS`; with no
/// trusted keys there is nothing to verify against, and unsigned arenas keep
/// working. An invalid signature is always refused. An unsigned head is
/// refused when `LEYLINE_HEAD_REQUIRE_SIGNATURE` is set.
pub fn enforce_signature(h: head::Reader<'_>, head_path: &Path) -> Result<()> {
    let trusted = crate::cmd_parse::head_trusted_keys_from_env()?;
    if trusted.is_empty() {
        return Ok(());
    }
    match verdict_of(h, &trusted)? {
        HeadVerdict::Valid => Ok(()),
        // Never acceptable: the head was edited after signing, corrupted,
        // signed by a key this reader does not trust, signed without the
        // treeRoot/parserId the signature must cover, or signed under the
        // removed v1 digest.
        HeadVerdict::Invalid => bail!(
            "head at {} failed signature verification (the signature must cover generation, rootHash, parentHash, treeRoot and parserId under head digest v2; a head signed before v2 must be re-signed by reparsing after deleting it)",
            head_path.display()
        ),
        HeadVerdict::Unsigned => {
            if std::env::var("LEYLINE_HEAD_REQUIRE_SIGNATURE")
                .is_ok_and(|v| matches!(v.trim(), "1" | "true"))
            {
                bail!(
                    "head at {} is unsigned and LEYLINE_HEAD_REQUIRE_SIGNATURE is set",
                    head_path.display()
                );
            }
            Ok(())
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

    #[test]
    fn a_stamped_head_reads_as_its_claim() {
        let got = claim_from(&build(|_| {})).unwrap();
        let want = HeadClaim::new(
            3,
            Hash::from_bytes([1u8; 32]),
            Hash::ZERO,
            Hash::from_bytes([4u8; 32]),
            Hash::from_bytes([6u8; 32]),
        );
        assert_eq!(got, want);
        assert!(got.is_some());
    }

    /// Unstamped (pre-ADR-0040) heads have no claim; the signature verifier
    /// then refuses any signature on them.
    #[test]
    fn an_unstamped_head_has_no_claim() {
        let no_tree = build(|h| h.reborrow().init_tree_root().set_bytes(&[]));
        assert_eq!(claim_from(&no_tree).unwrap(), None);
        let zero_parser = build(|h| h.reborrow().init_parser_id().set_bytes(&[0u8; 32]));
        assert_eq!(claim_from(&zero_parser).unwrap(), None);
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
