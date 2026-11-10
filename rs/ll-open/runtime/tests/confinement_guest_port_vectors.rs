//! The implementation half of `confinement/v1/test-vectors/manifest-guest-port.json`.
//!
//! ## Why (bead `ley-line-open-be3711`, ADR-0046 Q7)
//!
//! §6 grants may carry `guestPort`, the guest loopback port where the microVM
//! tier presents the channel to an unmodified workload. The issuer chooses it
//! and the digest commits to it, so `ConfinementManifest` must reach exactly
//! the canonical bytes and digests the spec pins, keep a grant without
//! `guestPort` in its one pre-extension spelling, and refuse every reject.

use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use serde_json::Value;

fn vectors() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/confinement/v1/test-vectors/manifest-guest-port.json");
    let raw = std::fs::read_to_string(path).expect("the guest-port vector file");
    serde_json::from_str(&raw).expect("vector file is valid JSON")
}

fn section<'a>(vectors: &'a Value, key: &str) -> &'a [Value] {
    vectors[key]
        .as_array()
        .unwrap_or_else(|| panic!("vector file must carry `{key}`"))
}

#[test]
fn every_case_reaches_its_canonical_bytes_and_digest() {
    let vectors = vectors();
    for case in section(&vectors, "cases") {
        let name = case["name"].as_str().expect("named");
        let manifest = ConfinementManifest::parse(&case["input"].to_string())
            .unwrap_or_else(|error| panic!("case {name} must parse: {error}"));
        let canonical = manifest.to_canonical_json().expect("canonicalizes");
        assert_eq!(
            canonical,
            case["canonical"].as_str().expect("canonical"),
            "case {name}: canonical bytes differ from the vector"
        );
        assert_eq!(
            manifest.confinement_digest().expect("digests"),
            case["confinementDigest"].as_str().expect("digest"),
            "case {name}: confinementDigest differs from the vector"
        );
        // The canonical bytes are a fixed point: parsing them back yields the
        // same manifest.
        assert_eq!(
            ConfinementManifest::parse(&canonical).expect("canonical parses"),
            manifest,
            "case {name}"
        );
    }
}

#[test]
fn every_reject_is_refused_by_the_parser() {
    let vectors = vectors();
    for reject in section(&vectors, "rejects") {
        let name = reject["name"].as_str().expect("named");
        let error = ConfinementManifest::parse(&reject["input"].to_string())
            .expect_err(&format!("reject {name} must be refused"));
        assert!(
            error.to_string().contains("§6") || error.to_string().contains("unixSocket"),
            "reject {name}: the refusal must name the dimension: {error}"
        );
    }
}

/// The builder carries the same rules the parser does, so a programmatic
/// issuer cannot produce a document the parser refuses.
#[test]
fn the_builder_refuses_what_the_parser_refuses() {
    assert!(UnixSocketGrant::connect("/s").with_guest_port(0).is_err());
    assert!(
        UnixSocketGrant::connect_bind("/s")
            .with_guest_port(13306)
            .is_err()
    );
    let dial = UnixSocketGrant::connect("/a")
        .with_guest_port(13306)
        .expect("a dial presentation");
    assert_eq!(dial.guest_port(), Some(13306));
    assert_eq!(UnixSocketGrant::connect("/a").guest_port(), None);
    assert_eq!(UnixSocketGrant::connect_bind("/a").guest_port(), None);
    let serve = UnixSocketGrant::bind("/b")
        .with_guest_port(13306)
        .expect("a serve presentation");
    assert_eq!(serve.guest_port(), Some(13306));
    let error = ConfinementManifest::new()
        .with_unix_socket(dial)
        .and_then(|manifest| manifest.with_unix_socket(serve))
        .expect_err("one guest port, two grants");
    assert!(error.to_string().contains("13306"), "{error}");
}
