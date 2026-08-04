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

use leyline_runtime::backends::libkrun::confinement::capabilities_from_manifest;
use leyline_runtime::confinement::{ConfinementManifest, FsGrant};

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
    capabilities_from_manifest(manifest, run_rootfs())
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
