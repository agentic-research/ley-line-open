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

use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, vsock_unix_mappings};
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
        .expect("valid credential source")
        .with_fs_grant(FsGrant::read_only("/etc/hosts"))
        .expect("valid fs grant")
        .with_fs_grant(FsGrant::read_write("/var/lib/bundle-X/"))
        .expect("valid fs grant")
        .with_allowed_host("*.telemetry.example.com")
        .expect("valid host")
        .with_allowed_host("api.example.com")
        .expect("valid host")
        .with_port_bind(8443, Some("127.0.0.1"))
        .expect("valid port")
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

    let widened = canonical_manifest()
        .with_fs_grant(FsGrant::read_only("/"))
        .expect("valid fs grant");

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
        .expect("valid credential source")
        .with_fs_grant(FsGrant::read_only("/etc/hosts"))
        .expect("valid fs grant")
        .with_allowed_host("api.example.com")
        .expect("valid host")
        .with_port_bind(8443, Some("127.0.0.1"))
        .expect("valid port");

    assert!(
        narrowed.assert_matches(&committed).is_err(),
        "the commitment is an equality, not an upper bound"
    );
}

/// Finding 2's other half: the backend must declare the digest of the policy
/// it *actually compiles*, not a digest handed to it.
///
/// `build_process_capabilities` used to construct a `nono::CapabilitySet`
/// directly from paths, so there was no manifest, no digest, and nothing the
/// grant's `confinementDigest` could be compared against. ADR-0035 §1 makes
/// the manifest the single declaration that both the applied CapabilitySet
/// and the declared digest are projections of — which is only true if the
/// manifest is the input to the CapabilitySet, not a description written
/// beside it.
///
/// This asserts the manifest names exactly what the backend was asked to
/// grant. If the two could be edited independently, the receipt would attest
/// a policy nobody applied.
#[test]
fn the_declared_manifest_names_exactly_what_the_backend_compiles() {
    let manifest = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[std::path::PathBuf::from("/usr/lib/libkrun.dylib")],
        &[std::path::PathBuf::from("/dev/kvm")],
        None,
    )
    .expect("valid manifest");

    let granted: Vec<&str> = manifest.fs_grants().iter().map(FsGrant::path).collect();
    assert_eq!(
        granted,
        vec![
            // The one writable tree, named SYMBOLICALLY. The realization is a
            // per-run temporary directory, and putting that in the attested
            // document would have made the digest unpredictable — see
            // `the_digest_does_not_move_with_the_ephemeral_root` below for why
            // that mattered. The trailing slash still marks a directory subtree.
            "/run/rootfs/",
            "/usr/lib/libkrun.dylib",
            "/dev/kvm",
        ],
        "the manifest must name exactly the paths the backend grants"
    );

    // No egress: confinement/v1 §3 makes an omitted `network` block "no
    // egress at all", which is what `block_network()` does.
    assert!(
        !manifest
            .to_canonical_json()
            .expect("canonical")
            .contains("allowHosts"),
        "a policy that blocks the network must not declare allowed hosts"
    );
}

/// The declared digest must move when the compiled policy moves. A digest
/// that stayed constant across different policies would satisfy any
/// commitment, which is the failure the whole mechanism exists to prevent.
#[test]
fn a_different_compiled_policy_yields_a_different_declared_digest() {
    let a = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[std::path::PathBuf::from("/usr/lib/libkrun.dylib")],
        &[],
        None,
    )
    .expect("valid manifest");
    let b = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[std::path::PathBuf::from("/usr/lib/libkrun.dylib")],
        &[std::path::PathBuf::from("/dev/kvm")],
        None,
    )
    .expect("valid manifest");

    assert_ne!(
        a.confinement_digest().expect("digest a"),
        b.confinement_digest().expect("digest b"),
        "granting a device must not share a digest with not granting it"
    );
}

/// The digest must NOT move when only the ephemeral realization moves — and
/// this is the property that makes `RunGrant.confinementDigest` usable at all.
///
/// The manifest used to name the materialized rootfs directly, and that path is
/// `<run_root>/rootfs` where `run_root` is a fresh `leyline-run-XXXXXX` tempdir
/// per run. So every run produced a different digest, and no issuer could ever
/// commit to one: the drift check both backends perform could only reject.
/// Nothing caught it because every real-worker test passed
/// `confinement_digest: String::new()`, which skips the comparison entirely.
///
/// Naming the root symbolically is what closes that. Coverage is not lost — the
/// rootfs CONTENT is attested separately by `ResolvedRootfs.digest`, verified by
/// `verify_ephemeral_rootfs` before anything runs, and reaches the receipt as
/// `inputRoots`. The host path is where those already-verified bytes were put.
#[test]
fn the_digest_does_not_move_with_the_ephemeral_root() {
    let first =
        leyline_runtime::backends::libkrun::confinement::confinement_manifest(&[], &[], None)
            .expect("valid manifest");
    let second =
        leyline_runtime::backends::libkrun::confinement::confinement_manifest(&[], &[], None)
            .expect("valid manifest");

    assert_eq!(
        first.confinement_digest().expect("first digest"),
        second.confinement_digest().expect("second digest"),
        "two runs of the same policy must produce the same digest, or no \
         issuer can commit to it and the drift check can only ever reject"
    );
    assert!(
        !first
            .to_canonical_json()
            .expect("canonical")
            .contains("leyline-run-"),
        "the attested document must not carry a per-run temporary path"
    );
}

/// The wire form round-trips through the type that computes the attested bytes.
///
/// `ConfinementManifest` used to be write-only — canonical JSON out, nothing in
/// — so a grant's confinement/v1 document could never become the manifest LLO
/// compiles. §4 therefore had no route to originate from an authorizer, and the
/// microVM tier's listener handling was unreachable even after it was written.
///
/// Round-trip over the pinned cross-impl vector, so this asserts against the
/// document both implementations agree on rather than one we made up.
#[test]
fn the_pinned_vector_parses_back_into_the_manifest_that_produced_it() {
    let pinned = std::fs::read_to_string(vector_path("test-vectors/manifest-canonical.json"))
        .expect("the pinned canonical manifest");

    let parsed = ConfinementManifest::parse(&pinned).expect("the pinned vector must parse");

    assert_eq!(
        parsed.to_canonical_json().expect("re-serialize"),
        pinned,
        "parse and serialize must be inverses, or a grant's document and the \
         digest LLO computes over it are two different things"
    );
    assert_eq!(
        parsed.confinement_digest().expect("digest"),
        canonical_manifest().confinement_digest().expect("digest"),
        "a parsed manifest and the programmatically-built one must agree"
    );
}

/// Parsing applies the SAME refusals as the builders, so there is no path that
/// produces a manifest the constructors would have rejected.
///
/// This is what lets the schema's refusal table be asserted against the Rust
/// type that computes the attested bytes, rather than only against `json!`
/// literals in a different crate — which is what §8's cross-impl conformance
/// claim actually rests on.
#[test]
fn parsing_refuses_every_document_the_schema_refuses() {
    // Each document carries a valid `version`, so the ONLY thing wrong with it
    // is the constraint it is meant to exercise. Without that these all passed
    // by being refused for a missing version — a test green for a reason it did
    // not name, which is the same defect this file exists to catch.
    const V: &str = r#""version":"cloister/confinement/v1""#;
    let refusals = [
        (
            format!(r#"{{{V},"port":{{"bind":0}}}}"#),
            "port 0 — nono's localhost:* wildcard",
        ),
        (
            format!(r#"{{{V},"port":{{"bind":80}}}}"#),
            "privileged port, §4 minimum is 1024",
        ),
        (
            format!(r#"{{{V},"fs":{{"allow":["relative/path"]}}}}"#),
            "relative path",
        ),
        (
            format!(r#"{{{V},"fs":{{"allow":["/srv/../etc"]}}}}"#),
            "`..` traversal",
        ),
        (
            format!(r#"{{{V},"network":{{"allowHosts":["api.*.example.com"]}}}}"#),
            "interior wildcard",
        ),
        (
            format!(r#"{{{V},"credentialSource":"https://vault/x"}}"#),
            "scheme outside the closed set",
        ),
        (
            format!(r#"{{{V},"gpu":{{"devices":[]}}}}"#),
            "unknown dimension",
        ),
        (
            format!(r#"{{{V},"fs":{{"allow":[{{"path":"/x","mode":"rx"}}]}}}}"#),
            "bogus mode",
        ),
        (
            r#"{"version":"cloister/confinement/v2","fs":{"allow":["/x"]}}"#.to_owned(),
            "a version this reader does not implement",
        ),
        (r#"{"fs":{"allow":["/x"]}}"#.to_owned(), "no version at all"),
    ];

    // And the same document WITHOUT its defect must parse, so each case above
    // is refused for its own reason rather than for something shared.
    ConfinementManifest::parse(&format!(r#"{{{V},"port":{{"bind":8443}}}}"#))
        .expect("an otherwise-identical conformant document must parse");

    for (document, case) in refusals {
        assert!(
            ConfinementManifest::parse(&document).is_err(),
            "parse must refuse {case}: {document}"
        );
    }
}

/// §4 reaches the tier that enforces it, and does so INSIDE the digested
/// object.
///
/// Before this, the listener dimension was declarable, digest-verified, and
/// undeliverable: `authorization.rs` parsed the grant's document and refused
/// any whose digest disagreed, then dropped it — the worker compiled its own
/// manifest, which never sets a port, so `dimensions().port` was permanently
/// `None` and the branch that writes libkrun's port map was unreachable. The
/// run was refused by the drift check rather than silently widened, so nothing
/// was ever attested that did not take effect; the dimension simply could not
/// succeed. That is the whole of what "`port.bind` is inert" meant.
///
/// The listener is taken INTO the manifest rather than applied beside it,
/// which is what keeps ADR-0035 §1 true — the digest the worker attests is
/// computed over the document that carries the port, so the receipt commits to
/// the listener instead of to a policy that omits it. This asserts the digest
/// MOVES when a listener is declared, because a digest that stayed constant
/// across different policies would satisfy any commitment.
#[test]
fn an_authorized_listener_reaches_the_compiled_manifest_and_moves_its_digest() {
    let runtime_files = [std::path::PathBuf::from("/usr/lib/libkrun.dylib")];

    let without = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &runtime_files,
        &[],
        None,
    )
    .expect("valid manifest");
    assert_eq!(
        without.dimensions().port,
        None,
        "a run with no authorized document must compile exactly the policy it \
         compiled before this field existed"
    );

    // The full document, exactly as an issuer commits to it: LLO's own policy
    // for this deployment plus the listener. Partial documents are refused by
    // the equality contract — a carried document is what the digest covers, so
    // it has to BE the policy, not a fragment of one.
    let authorized = ConfinementManifest::new()
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_write(
            leyline_runtime::backends::libkrun::confinement::ATTESTED_RUN_ROOTFS,
        ))
        .expect("rootfs grant")
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_only(
            "/usr/lib/libkrun.dylib",
        ))
        .expect("runtime file grant")
        .with_port_bind(8443, None)
        .expect("8443 is a legal §4 port");
    let with = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &runtime_files,
        &[],
        Some(&authorized),
    )
    .expect("valid manifest");

    assert_eq!(
        with.dimensions().port,
        Some((8443, None)),
        "the listener the grant authorized must be the listener the tier compiles"
    );
    assert_ne!(
        without.confinement_digest().expect("digest without"),
        with.confinement_digest().expect("digest with"),
        "declaring a listener must move the digest — a digest that did not \
         would let one commitment satisfy two different policies"
    );
}

/// The listener may originate ONLY from the authorized document.
///
/// `plan.rs` refuses to derive one from an `ExecutionRequest` because "a
/// workload does not get to widen its own boundary", and names the manifest
/// the grant authorized as the only source. This pins the other half: an
/// authorized document carrying no §4 block does not acquire one, so passing a
/// document through cannot silently open a port that nobody declared.
#[test]
fn an_authorized_document_without_a_listener_does_not_acquire_one() {
    // LLO's exact document for an empty deployment — the rootfs grant and
    // nothing else. That is what "declares no listener" means for a carried
    // document: identical to the compiled policy, minus nothing.
    let authorized = ConfinementManifest::new()
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_write(
            leyline_runtime::backends::libkrun::confinement::ATTESTED_RUN_ROOTFS,
        ))
        .expect("rootfs grant");
    let compiled = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[],
        &[],
        Some(&authorized),
    )
    .expect("valid manifest");

    assert_eq!(
        compiled.dimensions().port,
        None,
        "a document that declares no listener must not produce one"
    );
    assert_eq!(
        compiled.confinement_digest().expect("digest"),
        leyline_runtime::backends::libkrun::confinement::confinement_manifest(&[], &[], None)
            .expect("valid manifest")
            .confinement_digest()
            .expect("digest"),
        "carrying a document that declares nothing must digest identically to \
         carrying no document at all"
    );
}

/// The case cloister found before any test did: a carried dimension the fold
/// does not deliver, refused at compile time with the dimension named.
///
/// Their original scenario was a §6 grant — which the fold has since learned
/// to deliver on both tiers, so this test moved to §3, one of the two
/// dimensions that still cannot originate from a grant (§3 needs the proxy
/// path; §5 has no reader on `apply_auto` at all). The property under test is
/// unchanged and is §9 condition 6 applied to a narrower commitment: before
/// this check, a carried undeliverable clause was parsed, digest-verified,
/// authorized, then dropped by the fold — and surfaced only as a bare
/// "confinement drift" digest mismatch from the supervisor, an error naming
/// neither the dimension nor the reason.
#[test]
fn a_carried_dimension_the_fold_does_not_deliver_is_refused_by_name() {
    let authorized = ConfinementManifest::new()
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_write(
            leyline_runtime::backends::libkrun::confinement::ATTESTED_RUN_ROOTFS,
        ))
        .expect("rootfs grant")
        .with_allowed_host("api.example.com")
        .expect("a legal §3 host");

    let error = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[],
        &[],
        Some(&authorized),
    )
    .expect_err("a carried §3 cannot take effect on this tier and must be refused");
    let message = error.to_string();
    assert!(
        message.contains("§3 network.allowHosts"),
        "the refusal must name the dimension the issuer committed to: {message}"
    );
    assert!(
        !message.contains("§2") && !message.contains("§4 port.bind") && !message.contains("§6"),
        "and must not name dimensions that agree: {message}"
    );
}

/// The other half of the equality contract: a carried document missing a grant
/// the compiled policy carries is refused too, not run narrower than signed.
///
/// Equality rather than subset is deliberate. A document missing the runtime
/// file grants would digest differently from the applied policy, so the drift
/// check would refuse it anyway — but as a bare mismatch. Worse, treating the
/// carried document as a lower bound would let a run proceed under filesystem
/// authority its issuer never saw. Both directions of disagreement are the
/// same defect: the commitment and the policy are not one object.
#[test]
fn a_carried_document_missing_a_compiled_grant_is_refused_by_name() {
    // LLO's document for a deployment WITH a runtime file — but the issuer
    // committed to a document without it.
    let authorized = ConfinementManifest::new()
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_write(
            leyline_runtime::backends::libkrun::confinement::ATTESTED_RUN_ROOTFS,
        ))
        .expect("rootfs grant");

    let error = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[std::path::PathBuf::from("/usr/lib/libkrun.dylib")],
        &[],
        Some(&authorized),
    )
    .expect_err("a document narrower than the compiled policy must be refused");
    let message = error.to_string();
    assert!(
        message.contains("§2 fs.allow"),
        "the refusal must name the filesystem dimension: {message}"
    );
}

/// ADR-0036 O2's macOS half: a §6 grant reaches the compiled manifest on the
/// NATIVE tier, where the confined process is the workload itself.
///
/// This is the dimension cloister's shim needs, and their harness-sandbox
/// names the design this test pins: Seatbelt grants network-bind and
/// network-inbound UNQUALIFIED whenever localhost TCP is allowed at all, so
/// their TCP shim channel rides an acknowledged un-enforced hole
/// (CLOISTER_ACCEPT_UNENFORCED_BIND) — while "a connect-only UDS grant IS
/// enforceable where a port is not". A folded §6 connect closes the hole
/// instead of acknowledging it: the workload may dial the one socket the
/// issuer named, and holds no TCP capability at all.
///
/// The fold is tier-scoped on purpose, and the companion test below pins the
/// other side: on the microVM tier the confined process is the VMM host, not
/// the workload, so a §6 grant there would confine the wrong process and the
/// named refusal stays.
#[test]
fn a_unix_socket_grant_reaches_the_native_tier_and_moves_its_digest() {
    let authorized = ConfinementManifest::new()
        .with_fs_grant(leyline_runtime::confinement::FsGrant::read_write(
            leyline_runtime::backends::libkrun::confinement::ATTESTED_RUN_ROOTFS,
        ))
        .expect("rootfs grant")
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::connect(
            "/run/cloister/shim.sock",
        ))
        .expect("a legal §6 grant");

    let compiled = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        &[],
        &[],
        Some(&authorized),
    )
    .expect("the native tier delivers §6 to the workload");

    assert_eq!(
        compiled.unix_sockets(),
        authorized.unix_sockets(),
        "the socket the issuer committed to must be the socket the tier compiles"
    );
    assert_eq!(
        compiled, authorized,
        "carried and compiled must be ONE object — the equality contract, satisfied"
    );
    assert_ne!(
        compiled.confinement_digest().expect("digest with §6"),
        leyline_runtime::backends::libkrun::confinement::confinement_manifest(&[], &[], None)
            .expect("valid manifest")
            .confinement_digest()
            .expect("digest without §6"),
        "declaring a socket must move the digest — the receipt commits to the channel"
    );
}

/// ADR-0036 O2's microVM half: §6 delivered as vsock↔socket mappings.
///
/// On this tier the confined process is the VMM host; the workload runs in
/// the guest and reaches host sockets only through the mappings constructed
/// here — "only what was constructed exists" IS the enforcement mechanism.
/// The pairing is a pure function of document order (grant `i` owns ports
/// `BASE+2i` dial / `BASE+2i+1` serve), so the attested digest already
/// covers every mapping and the receipt needs no new field. An earlier
/// version of this test pinned the named refusal that stood here before the
/// mapping existed.
#[test]
fn a_socket_grant_becomes_exactly_the_vsock_mappings_its_modes_permit() {
    // A bound endpoint for the connect grant's ordering contract — the test
    // binary itself, same stand-in `unix_socket_mode`'s tests use: the check
    // reads `Path::exists`, not "is a socket", and the binary cannot race or
    // leak.
    let bound = std::env::current_exe()
        .expect("test binary has a path")
        .display()
        .to_string();

    let manifest = ConfinementManifest::new()
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::connect(
            &bound,
        ))
        .expect("connect grant")
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::bind(
            "/run/llo/served.sock",
        ))
        .expect("bind grant")
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::connect_bind(
            "/run/llo/both.sock",
        ))
        .expect("connect-bind grant");

    let mappings = vsock_unix_mappings(&manifest).expect("every mode maps");
    let shape: Vec<(u32, String, bool)> = mappings
        .iter()
        .map(|m| (m.port, m.host_path.to_string_lossy().into_owned(), m.listen))
        .collect();
    assert_eq!(
        shape,
        vec![
            // Grant 0, connect: the dial port only.
            (VSOCK_UNIX_BASE_PORT, bound, false),
            // Grant 1, bind: the serve port only — the mode the NATIVE tier
            // cannot express at all, deliverable here because the withhold is
            // the muxer's reset, not a filter.
            (
                VSOCK_UNIX_BASE_PORT + 3,
                "/run/llo/served.sock".into(),
                true
            ),
            // Grant 2, connect-bind: both halves, each on its own port.
            (VSOCK_UNIX_BASE_PORT + 4, "/run/llo/both.sock".into(), false),
            (VSOCK_UNIX_BASE_PORT + 5, "/run/llo/both.sock".into(), true),
        ],
        "grant i owns ports BASE+2i / BASE+2i+1 — a pure function of document \
         order, so the attested digest already covers every mapping"
    );
}

/// The two microVM refusals that remain, each naming its reason.
#[test]
fn the_microvm_mappings_refuse_what_a_mapping_cannot_express() {
    // A directory grant names a tree; a mapping needs a leaf.
    let directory = ConfinementManifest::new()
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::connect(
            "/run/llo/sockets/",
        ))
        .expect("directory grant");
    let error = vsock_unix_mappings(&directory).expect_err("a tree has no endpoint to map");
    assert!(
        error.to_string().contains("directory grant"),
        "the refusal must say why: {error}"
    );

    // The §6 ordering contract, same as the native tier: a connect grant
    // names an endpoint someone else owns, bound before the workload starts.
    let unbound = ConfinementManifest::new()
        .with_unix_socket(leyline_runtime::confinement::UnixSocketGrant::connect(
            "/run/llo/nobody-bound-this.sock",
        ))
        .expect("connect grant");
    let error = vsock_unix_mappings(&unbound).expect_err("nothing is bound at the endpoint");
    assert!(
        error.to_string().contains("nothing is bound there yet"),
        "the refusal must name the ordering contract: {error}"
    );
}

/// §3 `connectLocal` is ADDITIVE: the pinned v1 vector does not move.
///
/// This is the claim every downstream consumer relies on when a dimension is
/// added — cloister's pinned vector and their `confinement_digest.rs`
/// conformance test must not need touching. It held for §6 in v0.16.0 and it
/// has to hold here, so it is asserted rather than assumed: absent fields are
/// omitted from canonical bytes, and a manifest declaring no `network` block
/// emits none.
#[test]
fn adding_the_local_connect_dimension_does_not_move_the_pinned_vector() {
    assert_eq!(
        canonical_manifest()
            .confinement_digest()
            .expect("digest the pinned manifest"),
        format!("blake3-256:{CANONICAL_PIN}"),
        "the canonical vector's digest must be unchanged by a new dimension it \
         does not declare"
    );
}

/// A declared `connectLocal` moves the digest and survives a round trip.
///
/// Two properties in one, because they fail differently: a digest that did not
/// move would let one commitment satisfy two policies, and bytes that did not
/// round-trip would mean an issuer and a runner computing different documents
/// from the same declaration.
#[test]
fn a_local_connect_grant_moves_the_digest_and_round_trips() {
    let base = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_only("/etc/hosts"))
        .expect("fs grant");
    let with = base
        .clone()
        .with_connect_local(8443)
        .expect("legal loopback target");

    assert_ne!(
        base.confinement_digest().expect("digest without"),
        with.confinement_digest().expect("digest with"),
        "declaring a channel must move the digest the receipt commits to"
    );

    let canonical = with.to_canonical_json().expect("canonical bytes");
    assert!(
        canonical.contains("connectLocal"),
        "the clause must appear under the network block: {canonical}"
    );
    assert_eq!(
        ConfinementManifest::parse(&canonical).expect("re-parse"),
        with,
        "parse must reconstruct exactly the manifest that produced the bytes"
    );
}

/// An empty `network` block is refused rather than digesting as a declaration.
///
/// §1's rule is that an omitted block IS the refusal, so `{"network": {}}`
/// would be a document committing bytes while granting nothing — the
/// declared-but-dead shape, one level up.
#[test]
fn an_empty_network_block_is_refused() {
    let error = ConfinementManifest::parse(r#"{"version":"cloister/confinement/v1","network":{}}"#)
        .expect_err("an empty network block declares nothing");
    assert!(
        error.to_string().contains("network"),
        "the refusal must name the block: {error}"
    );
}
