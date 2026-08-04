//! The kernel-confinement assertions ported from cloister's
//! `tools/harness-sandbox` (bead `ley-line-open-704853`).
//!
//! ## What moved, and what did not
//!
//! The harness carries five assertions. Three are LLO's, two are not:
//!
//! | harness test | here? | why |
//! |---|---|---|
//! | `canonicalizer_reproduces_llo_v1_pin` | **yes** | LLO hashed the vector *file*; it had no serializer that produces those bytes. Hashing a file proves the file has not changed, not that we can reproduce it. |
//! | `accepts_when_manifest_matches_commitment` | **yes** | This is PR #312 finding 2 — `confinementDigest` on the wire and never enforced. |
//! | `rejects_on_manifest_tamper` | **yes** | Same finding, failing direction: a widened policy must be refused, not applied. |
//! | `rejects_when_cert_commits_no_digest` | no | Verifies a Signet bridge cert. In LLO that is `actorProvenanceEvidence` — the embedder's `EvidenceVerifier`, and LLO deliberately owns no trust roots. |
//! | `rejects_when_master_pubkey_wrong` | no | Same: a cert-chain check against a master key LLO does not hold. |
//!
//! Porting the last two would mean asserting against a trust root LLO does
//! not have, which is a stub that reads like coverage.
//!
//! ## Why the drift check is the load-bearing one
//!
//! ADR-0035 §1: the manifest, the `nono::CapabilitySet` compiled from it, and
//! the digest a backend declares are projections of one object, so they
//! cannot drift. Before this, `build_process_capabilities` compiled a
//! hardcoded policy with no relationship to the `confinementDigest` the grant
//! named — so a grant could name any policy, the worker applied a different
//! one, and the receipt attested the named one. A verifier downstream got a
//! true answer to the wrong question.

use leyline_runtime::confinement::{ConfinementManifest, FsGrant};

/// The digest both implementations must reach, pinned in
/// `confinement/v1/CONFINEMENT_DIGESTS.blake3`.
const CANONICAL_PIN: &str = "d9b5b7270bb6e5ec068aec92798dd76b0f71d1fe2640b3a09833b7742d51c617";

fn vector_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/confinement/v1")
        .join(name)
}

/// The manifest of `test-vectors/manifest-canonical.json`, built from the
/// typed API rather than parsed from the file — so the assertion below is
/// about our *serializer*, not about `serde_json` round-tripping.
fn canonical_manifest() -> ConfinementManifest {
    ConfinementManifest::new()
        .with_credential_source("keychain://bundle-X-credentials")
        .with_fs_grant(FsGrant::read_only("/etc/hosts"))
        .with_fs_grant(FsGrant::read_write("/var/lib/bundle-X/"))
        .with_allowed_host("*.telemetry.example.com")
        .with_allowed_host("api.example.com")
        .with_port_bind(8443, Some("127.0.0.1"))
}

/// Cloister's `canonicalizer_reproduces_llo_v1_pin`, in the direction that
/// was missing here.
///
/// `verify_confinement_digest` already hashes the vector file and compares to
/// the pin — but that only proves the file has not drifted. It says nothing
/// about whether LLO can *produce* those bytes, which is what a backend must
/// do to declare the digest of the policy it compiled. Both halves are needed:
/// this asserts serializer → bytes, the schema-spec test asserts bytes →
/// digest.
#[test]
fn our_canonicalizer_reproduces_the_pinned_vector_byte_for_byte() {
    let expected = std::fs::read_to_string(vector_path("test-vectors/manifest-canonical.json"))
        .expect("the pinned canonical manifest");
    let produced = canonical_manifest()
        .to_canonical_json()
        .expect("canonical serialization");

    assert_eq!(
        produced, expected,
        "our canonical serializer does not reproduce the pinned vector — \
         a backend cannot declare a digest it cannot compute"
    );
    assert_eq!(
        canonical_manifest()
            .confinement_digest()
            .expect("digest the manifest"),
        format!("blake3-256:{CANONICAL_PIN}"),
        "the reproduced bytes must reach the pinned BLAKE3 digest"
    );
}

/// Cloister's `accepts_when_manifest_matches_commitment`.
#[test]
fn a_manifest_matching_the_committed_digest_is_accepted() {
    let manifest = canonical_manifest();
    let committed = manifest.confinement_digest().expect("digest");

    manifest
        .assert_matches(&committed)
        .expect("a manifest that hashes to the committed digest must verify");
}

/// Cloister's `rejects_on_manifest_tamper`, with their exact widening: the
/// commitment covers the canonical policy, and the enforced policy adds `/`.
///
/// Root is the sharpest case — it is not a subtle widening, it grants the
/// whole filesystem — and it is what makes the failure mode concrete: without
/// this check the worker applies `/` while the receipt attests a policy that
/// allowed two paths.
#[test]
fn widening_the_enforced_policy_is_refused_as_drift() {
    let committed = canonical_manifest().confinement_digest().expect("digest");

    let widened = canonical_manifest().with_fs_grant(FsGrant::read_only("/"));

    let error = widened
        .assert_matches(&committed)
        .expect_err("a widened policy must not satisfy the narrower commitment");
    assert!(
        format!("{error:?}").contains("confinement drift"),
        "the rejection must name drift so an operator knows the policy \
         disagreed rather than that some digest mismatched: {error:?}"
    );
}

/// Narrowing must be refused too.
///
/// Cloister only tests widening, because widening is the attack. But the
/// commitment is an equality, not a bound: a runner that silently accepted a
/// *narrower* policy would emit a receipt attesting capabilities the workload
/// never had, which is a false attestation even though it is the safe
/// direction operationally.
#[test]
fn narrowing_the_enforced_policy_is_refused_too() {
    let committed = canonical_manifest().confinement_digest().expect("digest");

    let narrowed = ConfinementManifest::new()
        .with_credential_source("keychain://bundle-X-credentials")
        .with_fs_grant(FsGrant::read_only("/etc/hosts"))
        .with_allowed_host("api.example.com")
        .with_port_bind(8443, Some("127.0.0.1"));

    assert!(
        narrowed.assert_matches(&committed).is_err(),
        "the commitment is an equality, not an upper bound"
    );
}
