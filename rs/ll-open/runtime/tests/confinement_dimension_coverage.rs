//! Every dimension confinement/v1 defines must be enforced or refused — never
//! silently dropped.
//!
//! ## Why (bead `ley-line-open-17536d`)
//!
//! `capabilities_from_manifest` compiled `fs.allow` and read nothing else. A
//! manifest declaring `port.bind`, `network.allowHosts` or `credentialSource`
//! produced a `CapabilitySet` in which those clauses had no effect, and nothing
//! said so.
//!
//! The failure direction was closed — a dropped grant leaves the sandbox
//! stricter than the manifest describes — which is exactly why it survived
//! review. But ADR-0035's claim is that the attested digest describes the policy
//! actually applied, and a digest committing to inert clauses breaks that as
//! surely as an over-permissive grant would, just quietly.
//!
//! ## The first version of this file was itself incomplete
//!
//! It opened with the sentence above and covered three dimensions of four. §5
//! `credentialSource` was missing because the manifest had no accessor for it —
//! the compiler could not have refused it even if someone had thought to, and
//! the gap was invisible at the call site.
//!
//! That is why `ConfinementManifest::dimensions()` now returns a struct that
//! consumers destructure exhaustively: a fifth dimension is `error[E0027]` in
//! every backend rather than a clause somebody forgets. These tests are the
//! behavioural half; the type is the half that cannot be forgotten.

use std::path::Path;

use leyline_runtime::backends::libkrun::confinement::{Tier, capabilities_from_manifest};
use leyline_runtime::confinement::{ConfinementManifest, FsGrant, UnixSocketGrant};

/// §4 is enforceable on Landlock and not on Seatbelt, so the expected outcome
/// is genuinely platform-dependent. Stated once rather than inline everywhere.
const SEATBELT: bool = cfg!(target_os = "macos");

/// Where the symbolic run root binds for these cases.
///
/// None of them declare `ATTESTED_RUN_ROOTFS`, so the realization is irrelevant
/// to what they assert — but the compiler now demands one, which is itself the
/// point: a manifest cannot be compiled without saying where its root landed.
fn run_rootfs() -> &'static Path {
    Path::new("/")
}

fn compile(
    manifest: &ConfinementManifest,
) -> Result<nono::CapabilitySet, leyline_runtime::ExecutionError> {
    // Native: these tests are about WORKLOAD semantics — what a dimension
    // means for the process that holds it. The microVM projection has its own
    // pair below, because the two must be allowed to disagree (that is what
    // `Tier` exists to express).
    capabilities_from_manifest(manifest, run_rootfs(), Tier::Native)
}

/// §2 alone compiles everywhere — the baseline the other cases vary from.
#[test]
fn a_filesystem_only_manifest_compiles_and_blocks_the_network() {
    let capabilities = compile(
        &ConfinementManifest::new()
            .with_fs_grant(FsGrant::read_only("/usr/lib/"))
            .expect("valid fs grant"),
    )
    .expect("fs-only manifest compiles");

    assert!(capabilities.is_network_blocked());
    assert!(
        capabilities.localhost_ports().is_empty(),
        "a manifest with no port block must grant no port"
    );
}

/// §4 on Linux is the shape cloister-harness runs: deny everything, then open
/// exactly one loopback port to the vault-proxy shim.
///
/// On macOS it is refused, and the refusal is the point — see the next test.
#[test]
fn a_loopback_listener_is_granted_where_the_kernel_can_scope_it() {
    let manifest = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_only("/usr/lib/"))
        .expect("valid fs grant")
        .with_port_bind(8443, None)
        .expect("valid port");

    match compile(&manifest) {
        Ok(capabilities) => {
            assert!(!SEATBELT, "Seatbelt cannot scope a bind by port");
            // Through nono's typed accessor rather than its `Debug` output: a
            // substring check would also pass if "8443" appeared in a path, and
            // `Debug` is not a stable surface.
            assert_eq!(capabilities.localhost_ports(), &[8443]);
            assert!(
                capabilities.is_network_blocked(),
                "a listener grant must not open general network access — the \
                 whole point of the shim shape is deny-all plus one port"
            );
        }
        Err(error) => assert!(SEATBELT, "§4 must compile off Seatbelt: {error:?}"),
    }
}

/// §4 on macOS grants every listener or none, so it must grant none.
///
/// nono's own source says why, immediately before emitting it
/// (`sandbox/macos.rs:838`, `NetworkMode::Blocked` with localhost TCP — exactly
/// this configuration):
///
/// ```text
/// // Seatbelt cannot filter bind/inbound by port
/// (allow network-bind)
/// (allow network-inbound)
/// ```
///
/// Seatbelt scopes the OUTBOUND direction per port. §4 is about the bind
/// direction, where it is all-or-nothing. An earlier version of this code
/// granted the port here and refused only a non-loopback *address* — applying
/// the "compiles to the same rule, so refuse it" argument to the address axis on
/// the one platform where the port axis is the broken one.
#[test]
#[cfg(target_os = "macos")]
fn a_listener_is_refused_on_seatbelt_because_it_would_grant_every_listener() {
    let error = compile(
        &ConfinementManifest::new()
            .with_port_bind(8443, None)
            .expect("valid port"),
    )
    .expect_err("Seatbelt cannot scope a bind by port, so §4 must be refused");

    let detail = format!("{error:?}");
    assert!(
        detail.contains("Seatbelt") && detail.contains("every listener"),
        "the refusal must say what would actually be granted, not merely that \
         it is unsupported: {detail}"
    );
}

/// §4 requires exposure beyond loopback to be an explicit declaration, and no
/// tier here can honour it — Landlock filters on port, not on address, so
/// `0.0.0.0` and loopback compile identically.
#[test]
fn a_listener_beyond_loopback_is_refused_rather_than_quietly_narrowed() {
    let error = compile(
        &ConfinementManifest::new()
            .with_port_bind(8443, Some("0.0.0.0"))
            .expect("valid port"),
    )
    .expect_err("a non-loopback listener must not compile silently");

    assert!(
        format!("{error:?}").contains("§4"),
        "the refusal must name the dimension: {error:?}"
    );
}

/// §3 host-scoped egress rides nono's proxy path, not the `apply_auto` path
/// this compiles to. Declaring it previously produced a fully network-blocked
/// sandbox with no indication the hosts were ignored.
#[test]
fn declared_egress_hosts_are_refused_rather_than_silently_blocked() {
    let error = compile(
        &ConfinementManifest::new()
            .with_fs_grant(FsGrant::read_only("/usr/lib/"))
            .expect("valid fs grant")
            .with_allowed_host("api.anthropic.com")
            .expect("valid host"),
    )
    .expect_err("an unenforceable egress dimension must not compile silently");

    assert!(
        format!("{error:?}").contains("allowHosts"),
        "the refusal must name the dimension: {error:?}"
    );
}

/// §5, the dimension the first version of this file missed entirely.
///
/// A `CapabilitySet` carries no credential binding, so a declared vault backend
/// has no reader on this path. It was not merely unenforced before — it was
/// unreadable, with no accessor on the manifest at all.
#[test]
fn a_declared_credential_source_is_refused_rather_than_ignored() {
    let error = compile(
        &ConfinementManifest::new()
            .with_fs_grant(FsGrant::read_only("/usr/lib/"))
            .expect("valid fs grant")
            .with_credential_source("keychain://cloister/vault")
            .expect("valid credential source"),
    )
    .expect_err("an unenforceable credential dimension must not compile silently");

    let detail = format!("{error:?}");
    assert!(
        detail.contains("§5") && detail.contains("credentialSource"),
        "the refusal must name the dimension: {detail}"
    );
}

/// The manifest must not be constructible into a document its own schema
/// refuses — `confinement.schema.json` is what the §8 cross-impl conformance
/// claim rests on, and it was asserted only against `json!` literals in another
/// crate, never against the type that computes the attested bytes.
///
/// Port 0 is why this is a refusal and not a lint. nono documents it as the
/// macOS `localhost:*` wildcard, emitting
/// `(allow network-outbound (remote tcp "localhost:*"))` — so a bare `u16`
/// admitted a value whose compiled meaning is *every* localhost port, which is
/// the exact inverse of the single-port grant §4 describes. No in-repo caller
/// did this; `with_port_bind` is `pub` and any embedder could.
#[test]
fn the_builders_refuse_documents_the_schema_refuses() {
    let refusals: Vec<(&str, bool)> = vec![
        (
            "port 0 — nono's localhost:* wildcard",
            ConfinementManifest::new().with_port_bind(0, None).is_err(),
        ),
        (
            "privileged port — §4 minimum is 1024",
            ConfinementManifest::new().with_port_bind(80, None).is_err(),
        ),
        (
            "relative fs path — §2 AbsolutePath",
            ConfinementManifest::new()
                .with_fs_grant(FsGrant::read_only("relative/path"))
                .is_err(),
        ),
        (
            "traversing fs path — §2 rejects `..`",
            ConfinementManifest::new()
                .with_fs_grant(FsGrant::read_only("/srv/../etc"))
                .is_err(),
        ),
        (
            "interior wildcard host — §3 allows one leading `*.` only",
            ConfinementManifest::new()
                .with_allowed_host("api.*.example.com")
                .is_err(),
        ),
        (
            "unclosed credential scheme — §5 enumerates them",
            ConfinementManifest::new()
                .with_credential_source("https://vault/x")
                .is_err(),
        ),
    ];

    for (case, refused) in refusals {
        assert!(refused, "the builder must refuse: {case}");
    }

    // And the conformant document still builds, so the refusals above are not
    // simply "everything fails".
    ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_write("/var/lib/bundle-X/"))
        .expect("absolute path")
        .with_allowed_host("*.telemetry.example.com")
        .expect("single leading wildcard")
        .with_credential_source("keychain://bundle-X-credentials")
        .expect("closed scheme")
        .with_port_bind(8443, Some("127.0.0.1"))
        .expect("in-range port");
}

/// The negative half of §4: an omitted `port` block means MUST NOT bind, so it
/// must not quietly become a grant.
#[test]
fn a_manifest_declaring_no_listener_grants_no_port() {
    let without = compile(&ConfinementManifest::new()).expect("empty manifest compiles");
    assert!(without.localhost_ports().is_empty());
    assert!(without.tcp_bind_ports().is_empty());
}

/// The boundaries themselves, because cargo-mutants showed the comparisons were
/// unobserved: `replace < with <=` in `with_port_bind` and `replace > with >=`
/// in `with_credential_source` both survived. A test that only tries 80 and
/// 8443 never distinguishes `< 1024` from `<= 1024`.
#[test]
fn the_refusal_boundaries_are_exactly_where_the_spec_puts_them() {
    // §4: 1024 is the first permitted port, 1023 the last refused one.
    assert!(
        ConfinementManifest::new()
            .with_port_bind(1023, None)
            .is_err(),
        "1023 is privileged and must be refused"
    );
    assert!(
        ConfinementManifest::new()
            .with_port_bind(1024, None)
            .is_ok(),
        "1024 is the spec's minimum and must be permitted"
    );
    assert!(
        ConfinementManifest::new()
            .with_port_bind(u16::MAX, None)
            .is_ok(),
        "65535 is the spec's maximum and must be permitted"
    );

    // §5: the scheme must be followed by something. A bare scheme names no
    // vault, and `>` vs `>=` on the length check is exactly that distinction.
    assert!(
        ConfinementManifest::new()
            .with_credential_source("keychain://")
            .is_err(),
        "a scheme with an empty remainder names no vault"
    );
    assert!(
        ConfinementManifest::new()
            .with_credential_source("keychain://x")
            .is_ok(),
        "one character of remainder is a vault reference"
    );
}

/// `allowed_hosts` is read by the compiler to decide §3's refusal, so a mutant
/// replacing the accessor must not survive. Three of them did.
#[test]
fn the_hosts_accessor_reports_what_was_declared() {
    let manifest = ConfinementManifest::new()
        .with_allowed_host("api.example.com")
        .expect("valid host")
        .with_allowed_host("*.telemetry.example.com")
        .expect("valid wildcard");

    // Both the accessor and the destructured view, because they are separate
    // code paths and cargo-mutants killed neither when only one was asserted.
    assert_eq!(
        manifest.allowed_hosts(),
        ["api.example.com", "*.telemetry.example.com"],
        "the compiler decides §3 from this; a wrong answer here is a wrong policy"
    );
    assert_eq!(
        manifest.dimensions().allow_hosts,
        manifest.allowed_hosts(),
        "the destructured view and the accessor must agree"
    );
    assert!(
        ConfinementManifest::new()
            .dimensions()
            .allow_hosts
            .is_empty(),
        "an undeclared §3 must read as empty, not as a phantom grant"
    );
}

/// §6 `bind` is serve-without-dial: it grants `bind(2)` and *withholds*
/// `connect(2)`. A `CapabilitySet` has no mode carrying that — the pair is
/// `Connect | ConnectBind` — so the compiler must refuse it rather than reach
/// for `ConnectBind`, which would add back exactly the `connect(2)` the mode
/// exists to remove.
///
/// This is the §4 failure one dimension over: a declaration that names one
/// thing and permits another. The refusal is what keeps the attested digest
/// describing the policy actually applied.
#[test]
fn a_serve_only_socket_is_refused_rather_than_widened_to_dial() {
    let manifest = ConfinementManifest::new()
        .with_unix_socket(UnixSocketGrant::bind("/run/llo/shim.sock"))
        .expect("an absolute socket path is a valid §6 grant");

    let error = compile(&manifest).expect_err("bind withholds connect; no mode carries that");
    let rendered = error.to_string();
    assert!(
        rendered.contains("§6") && rendered.contains("bind"),
        "the refusal must name the dimension and the mode it could not honour, got: {rendered}"
    );
}

/// The positive half, so the test above cannot pass by refusing *everything*
/// in §6. On Seatbelt a dial-only grant compiles; on Landlock the whole
/// dimension is refused at the ABI this build targets, and that split is the
/// §1 table rather than an accident here.
///
/// # The ordering requirement is part of the contract
///
/// A `connect` grant only compiles once the endpoint is bound, because the path
/// is *resolved* when the grant is compiled — which is what stops a symlink
/// planted at the path from redirecting the grant somewhere else. So compiling
/// is a function of the manifest *and* the moment, and README §6 says so rather
/// than leaving it to be discovered.
///
/// The half that was genuinely wrong was the diagnostic: this surfaced as
/// `BackendFailed: ... Path does not exist`, which reads as an internal fault
/// and invites a caller to "fix" the manifest. It must name the dimension, the
/// path, and the ordering, and it must say the same document compiles unchanged
/// once the peer is up.
#[test]
fn a_dial_only_socket_compiles_where_the_mechanism_carries_it() {
    let existing = tempfile::tempdir().expect("tempdir");
    let socket_path = existing.path().join("vault-proxy.sock");
    std::os::unix::net::UnixListener::bind(&socket_path).expect("bind a real socket");

    let bound = ConfinementManifest::new()
        .with_unix_socket(UnixSocketGrant::connect(socket_path.to_string_lossy()))
        .expect("an absolute socket path is a valid §6 grant");
    let outcome = compile(&bound);
    assert_eq!(
        outcome.is_ok(),
        SEATBELT,
        "§6 connect is expressible on Seatbelt and unavailable at the targeted \
         Landlock ABI — see the §1 table. Got: {:?}",
        outcome.err().map(|e| e.to_string())
    );

    // Same manifest shape, same digest semantics, socket not yet bound. The
    // refusal must be legible on BOTH platforms: on Linux the whole dimension is
    // unavailable, on macOS it is the ordering contract. Neither may render as a
    // bare backend fault.
    let unbound = ConfinementManifest::new()
        .with_unix_socket(UnixSocketGrant::connect(
            existing.path().join("not-yet.sock").to_string_lossy(),
        ))
        .expect("an absolute socket path is a valid §6 grant");
    let rendered = compile(&unbound)
        .expect_err("a connect grant on an unbound endpoint cannot be resolved")
        .to_string();
    assert!(
        rendered.contains("§6"),
        "the refusal must name the dimension, not leak nono's wording: {rendered:?}"
    );
    if SEATBELT {
        assert!(
            rendered.contains("nothing is bound there yet")
                && rendered.contains("compiles once the peer is up"),
            "the refusal must state the ordering contract and that the manifest is \
             not at fault, so a caller does not go 'fix' a correct document: {rendered:?}"
        );
    }
}

/// The wire vocabulary is what v1 pins, so each mode must have exactly one
/// spelling and it must survive the round-trip the authorization path depends
/// on. A mode that canonicalizes to a string the parser rejects would digest
/// fine and fail to re-parse — the drift §7 exists to prevent.
#[test]
fn every_socket_mode_round_trips_through_its_one_spelling() {
    for grant in [
        UnixSocketGrant::connect("/run/llo/a.sock"),
        UnixSocketGrant::bind("/run/llo/b.sock"),
        UnixSocketGrant::connect_bind("/run/llo/c.sock"),
    ] {
        let manifest = ConfinementManifest::new()
            .with_unix_socket(grant.clone())
            .expect("valid §6 grant");
        let canonical = manifest
            .to_canonical_json()
            .expect("a valid manifest must canonicalize");
        let parsed =
            ConfinementManifest::parse(&canonical).expect("canonical bytes must parse back");
        assert_eq!(
            parsed.unix_sockets(),
            [grant.clone()],
            "mode {:?} did not survive the canonical round-trip",
            grant.mode()
        );
    }
}

/// The mode predicates decide both §6 refusals, so they are load-bearing on
/// every platform even though the compile path they feed is macOS-only. CI runs
/// on Linux, where §6 refuses at the top for the ABI reason and the per-grant
/// logic is never reached — which left `permits_bind`, `permits_connect` and
/// both refusal conditions unexercised there. cargo-mutants found exactly that:
/// seven survivors, all reachable only through a platform branch.
///
/// Asserting the predicates directly is platform-independent, so it closes the
/// gap without pretending the compile path is portable.
#[test]
fn the_mode_predicates_answer_for_each_mode_on_every_platform() {
    let connect = UnixSocketGrant::connect("/run/llo/a.sock");
    let bind = UnixSocketGrant::bind("/run/llo/b.sock");
    let connect_bind = UnixSocketGrant::connect_bind("/run/llo/c.sock");

    // permits_bind: false only for connect. A mutant returning a constant
    // either widens connect into a bind grant or strips bind from the two
    // modes that have it — both are policy changes, not refactors.
    assert!(!connect.permits_bind(), "connect must not permit bind(2)");
    assert!(bind.permits_bind(), "bind must permit bind(2)");
    assert!(
        connect_bind.permits_bind(),
        "connect-bind must permit bind(2)"
    );

    // permits_connect: false only for bind. This is the predicate the
    // serve-without-dial refusal reads, so a constant here either lets `bind`
    // compile as a dial grant or refuses the two modes that legitimately dial.
    assert!(connect.permits_connect(), "connect must permit connect(2)");
    assert!(
        !bind.permits_connect(),
        "bind withholds connect(2) — that is the whole mode"
    );
    assert!(
        connect_bind.permits_connect(),
        "connect-bind must permit connect(2)"
    );

    // The two predicates must not collapse into one. If they ever agree on
    // every mode, one of them is redundant and the refusals lose a distinction.
    assert_ne!(
        bind.permits_bind(),
        bind.permits_connect(),
        "bind is precisely where the two predicates disagree"
    );

    // And the wire spelling each mode reports, since `mode()` is the single
    // source consulted by canonical bytes, the schema and every diagnostic.
    assert_eq!(connect.mode(), "connect");
    assert_eq!(bind.mode(), "bind");
    assert_eq!(connect_bind.mode(), "connect-bind");
}

/// The tier projection, on the pair the platforms disagree hardest about.
///
/// The SAME §6-carrying manifest must be refused under `Tier::Native` on
/// Linux (Landlock at this ABI has no AF_UNIX right — the grant would compile
/// to nothing the workload is bound by) and accepted under `Tier::MicroVm` on
/// every platform (the workload's boundary there is the vsock mapping, and
/// this profile carries only the VMM's host half of the channel). One
/// document, two confined processes, two correct answers — which is exactly
/// what `Tier` exists to express, and what a tier-blind compiler cannot.
#[test]
fn the_microvm_projection_accepts_the_socket_grant_the_native_tier_refuses() {
    // A path whose PARENT exists: nono canonicalizes the parent at add time
    // (the §6 anti-symlink property), and a bind-capable grant tolerates a
    // missing leaf because bind creates it. In a deployment the socket's
    // directory exists for the same reason.
    let served = std::env::temp_dir().join("llo-vsock-served.sock");
    let manifest = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_write("/run/rootfs/"))
        .expect("rootfs grant")
        .with_unix_socket(UnixSocketGrant::bind(served.display().to_string()))
        .expect("a legal §6 grant");

    let microvm = capabilities_from_manifest(&manifest, run_rootfs(), Tier::MicroVm);
    assert!(
        microvm.is_ok(),
        "the VMM projection must accept §6 on every platform — the workload's \
         boundary is the vsock mapping, not this profile: {microvm:?}"
    );

    // Native: platform-split by design. Linux refuses the dimension (ABI);
    // macOS refuses this MODE (`bind` withholds connect(2), which no
    // UnixSocketMode expresses) — so the grant is refused on both, for two
    // different, correctly-named reasons.
    let native = compile(&manifest).expect_err("bind is refused under workload semantics");
    let message = native.to_string();
    #[cfg(target_os = "linux")]
    assert!(
        message.contains("Landlock ABI"),
        "Linux must refuse the dimension, naming the ABI: {message}"
    );
    #[cfg(not(target_os = "linux"))]
    assert!(
        message.contains("§6") && message.contains("bind"),
        "macOS must refuse the mode, naming it: {message}"
    );
}
