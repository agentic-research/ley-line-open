//! Every dimension confinement/v1 defines must be enforced or refused — never
//! silently dropped.
//!
//! ## Why (bead `ley-line-open-17536d`)
//!
//! `capabilities_from_manifest` compiled only `fs.allow`. A manifest declaring
//! `port.bind` or `network.allowHosts` produced a `CapabilitySet` in which those
//! dimensions had no effect, and nothing said so.
//!
//! The failure direction was closed — a dropped grant leaves the sandbox
//! stricter than the manifest describes, never looser — which is exactly why it
//! survived review. But ADR-0035's whole claim is that the attested confinement
//! digest describes the policy actually applied. A digest committing to a
//! document whose §3 and §4 had no effect breaks that claim just as surely as an
//! over-permissive grant would, and does it quietly.
//!
//! The existing tests could not catch this. `verify_confinement_schema` checks
//! the SCHEMA against the spec's prose; `capability_mapping_coverage` checks
//! that every spec *directory* has a crosswalk row. Neither binds the schema's
//! dimensions to the compiler that consumes them, so a dimension could be fully
//! specified, fully documented, and entirely inert. These tests are that
//! binding.

use leyline_runtime::backends::libkrun::confinement::capabilities_from_manifest;
use leyline_runtime::confinement::{ConfinementManifest, FsGrant};

/// §4's default is 127.0.0.1, and this is the shape cloister-harness runs
/// today: deny everything, then open exactly one loopback port to the
/// vault-proxy shim (`--block-net --open-port <shim>`).
#[test]
fn a_loopback_listener_is_granted_and_the_network_stays_blocked() {
    let manifest = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_only("/usr/lib/"))
        .with_port_bind(8443, None);

    let capabilities = capabilities_from_manifest(&manifest).expect("loopback listener compiles");

    assert!(
        capabilities.is_network_blocked(),
        "a listener grant must not open general network access — the whole \
         point of the shim shape is deny-all plus one port"
    );
    assert!(
        format!("{capabilities:?}").contains("8443"),
        "the granted port must reach the CapabilitySet, or the manifest \
         declared a listener that nothing enforces: {capabilities:?}"
    );
}

/// An explicit loopback address is the same grant as the default. Pinned so a
/// future refactor cannot start refusing the spelling §4 documents.
#[test]
fn an_explicitly_declared_loopback_address_is_equivalent_to_the_default() {
    let defaulted =
        capabilities_from_manifest(&ConfinementManifest::new().with_port_bind(9000, None))
            .expect("default address compiles");
    let explicit = capabilities_from_manifest(
        &ConfinementManifest::new().with_port_bind(9000, Some("127.0.0.1")),
    )
    .expect("explicit loopback compiles");

    assert_eq!(format!("{defaulted:?}"), format!("{explicit:?}"));
}

/// §4 requires exposure beyond loopback to be an explicit declaration. This
/// tier cannot honour that declaration — Landlock filters on port alone, so a
/// `0.0.0.0` grant and a loopback grant compile to the identical rule. Refusing
/// is the only answer that does not attest an exposure decision nothing
/// enforces.
#[test]
fn a_listener_beyond_loopback_is_refused_rather_than_quietly_narrowed() {
    let manifest = ConfinementManifest::new().with_port_bind(8443, Some("0.0.0.0"));

    let error = capabilities_from_manifest(&manifest)
        .expect_err("a non-loopback listener must not compile silently");

    let detail = format!("{error:?}");
    assert!(
        detail.contains("0.0.0.0") && detail.contains("bind address"),
        "the refusal must name the address it cannot enforce, so an operator \
         learns why rather than that: {detail}"
    );
}

/// §3 host-scoped egress rides nono's proxy path, not the `apply_auto` path
/// this compiles to. Declaring it here previously produced a fully
/// network-blocked sandbox with no indication the hosts were ignored.
#[test]
fn declared_egress_hosts_are_refused_rather_than_silently_blocked() {
    let manifest = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_only("/usr/lib/"))
        .with_allowed_host("api.anthropic.com");

    let error = capabilities_from_manifest(&manifest)
        .expect_err("an unenforceable egress dimension must not compile silently");

    let detail = format!("{error:?}");
    assert!(
        detail.contains("allowHosts"),
        "the refusal must name the dimension: {detail}"
    );
}

/// The negative half of §4: an omitted `port` block means MUST NOT bind, so it
/// must not quietly become a grant. Without this, "no listener" and "a listener
/// nobody enforced" are indistinguishable from the outside.
#[test]
fn a_manifest_declaring_no_listener_grants_no_port() {
    let with_port =
        capabilities_from_manifest(&ConfinementManifest::new().with_port_bind(7532, None))
            .expect("listener compiles");
    let without =
        capabilities_from_manifest(&ConfinementManifest::new()).expect("empty manifest compiles");

    assert_ne!(
        format!("{with_port:?}"),
        format!("{without:?}"),
        "granting a listener must be observable in the compiled policy; if these \
         are equal then with_port_bind changed nothing and §4 is inert"
    );
    assert!(
        !format!("{without:?}").contains("7532"),
        "a manifest with no port block must grant no port"
    );
}
