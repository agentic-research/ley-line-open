//! The implementation half of `confinement/v1/test-vectors/manifest-port-list.json`.
//!
//! ## Why (bead `ley-line-open-e2c33f`)
//!
//! §4 `port` and §3 `network.connectLocal` accept a list as well as a scalar.
//! The schema-spec crate pins the canonical bytes and digests the spec
//! prescribes; this file shows that `ConfinementManifest` reaches exactly
//! those bytes, computes the meet the vectors pin, and refuses what they say
//! to refuse. A parser that canonicalized a one-element list as a list, or
//! computed the meet as a union, fails here.

use leyline_runtime::confinement::ConfinementManifest;
use serde_json::Value;

/// `confinement/v1`'s pinned single-port digest. It must not move.
const CANONICAL_PIN: &str = "d9b5b7270bb6e5ec068aec92798dd76b0f71d1fe2640b3a09833b7742d51c617";

fn spec_path(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/confinement/v1")
        .join(rel)
}

fn vectors() -> Value {
    let raw = std::fs::read_to_string(spec_path("test-vectors/manifest-port-list.json"))
        .expect("the port-list vector file");
    serde_json::from_str(&raw).expect("vector file is valid JSON")
}

fn section<'a>(vectors: &'a Value, key: &str) -> &'a [Value] {
    vectors[key]
        .as_array()
        .unwrap_or_else(|| panic!("vector file must carry `{key}`"))
}

fn parse(value: &Value) -> ConfinementManifest {
    ConfinementManifest::parse(&value.to_string())
        .unwrap_or_else(|error| panic!("vector input must parse: {error}\n{value:#}"))
}

#[test]
fn every_case_reaches_its_canonical_bytes_and_digest() {
    let vectors = vectors();
    for case in section(&vectors, "cases") {
        let name = case["name"].as_str().expect("named");
        let manifest = parse(&case["input"]);
        assert_eq!(
            manifest.to_canonical_json().expect("canonicalizes"),
            case["canonical"].as_str().expect("canonical"),
            "case {name}: canonical bytes differ from the vector"
        );
        assert_eq!(
            manifest.confinement_digest().expect("digests"),
            case["confinementDigest"].as_str().expect("digest"),
            "case {name}: confinementDigest differs from the vector"
        );
    }
}

/// The digests-unchanged proof, through the implementation: the pinned
/// single-port vector, and the same grant written as a one-element list, both
/// reach the digest every v1 implementation already agrees on.
#[test]
fn the_pinned_single_port_digest_is_unchanged_by_the_list_form() {
    let pinned = std::fs::read_to_string(spec_path("test-vectors/manifest-canonical.json"))
        .expect("the pinned canonical manifest");
    let scalar = ConfinementManifest::parse(&pinned).expect("the pinned vector parses");
    assert_eq!(
        scalar.confinement_digest().expect("digest"),
        format!("blake3-256:{CANONICAL_PIN}")
    );
    assert_eq!(scalar.to_canonical_json().expect("canonical"), pinned);

    let mut as_list: Value = serde_json::from_str(&pinned).expect("JSON");
    let grant = as_list["port"].take();
    as_list["port"] = Value::Array(vec![grant]);
    let listed = parse(&as_list);
    assert_eq!(
        listed.confinement_digest().expect("digest"),
        format!("blake3-256:{CANONICAL_PIN}"),
        "a one-element list must digest exactly as the scalar it spells"
    );
    assert_eq!(listed, scalar, "and must be the same manifest");
}

/// The meet is the intersection, and it is commutative byte-for-byte.
///
/// `port-disjoint-is-zero-grants` is the case that separates intersection from
/// union: a union keeps both grants, an intersection keeps none.
#[test]
fn every_meet_is_the_intersection_in_either_order() {
    let vectors = vectors();
    for case in section(&vectors, "meets") {
        let name = case["name"].as_str().expect("named");
        let left = parse(&case["left"]);
        let right = parse(&case["right"]);
        for (order, meet) in [
            ("left ∧ right", left.meet(&right)),
            ("right ∧ left", right.meet(&left)),
        ] {
            let meet =
                meet.unwrap_or_else(|error| panic!("meet {name} ({order}) refused: {error}"));
            assert_eq!(
                meet.to_canonical_json().expect("canonical"),
                case["canonical"].as_str().expect("canonical"),
                "meet {name} ({order}): canonical bytes differ from the vector"
            );
            assert_eq!(
                meet.confinement_digest().expect("digest"),
                case["confinementDigest"].as_str().expect("digest"),
                "meet {name} ({order}): digest differs from the vector"
            );
        }
        // A meet only narrows: meeting the result with either operand again
        // changes nothing.
        let meet = left.meet(&right).expect("met above");
        assert_eq!(meet.meet(&left).expect("narrowing again"), meet, "{name}");
        assert_eq!(meet.meet(&right).expect("narrowing again"), meet, "{name}");
    }
}

#[test]
fn a_meet_across_differing_dimensions_is_refused_by_name() {
    let vectors = vectors();
    for case in section(&vectors, "meetRefusals") {
        let name = case["name"].as_str().expect("named");
        let naming = case["refusedNaming"].as_str().expect("refusedNaming");
        let left = parse(&case["left"]);
        let right = parse(&case["right"]);
        for meet in [left.meet(&right), right.meet(&left)] {
            let message = meet
                .expect_err(&format!("meet {name} must be refused"))
                .to_string();
            assert!(
                message.contains(naming),
                "meet {name}: refusal must name {naming}: {message}"
            );
        }
    }
}

#[test]
fn every_reject_is_refused_by_the_parser() {
    let vectors = vectors();
    for case in section(&vectors, "rejects") {
        let name = case["name"].as_str().expect("named");
        assert!(
            ConfinementManifest::parse(&case["input"].to_string()).is_err(),
            "parser accepted reject {name}: {:#}",
            case["input"]
        );
    }
}

/// The accessors report every grant, in canonical order — not the first.
#[test]
fn the_list_accessors_report_every_grant_in_canonical_order() {
    let manifest = ConfinementManifest::parse(
        r#"{"version":"cloister/confinement/v1",
            "port":[{"bind":9090},{"bind":8443,"address":"127.0.0.1"}],
            "network":{"connectLocal":[9443,8443]}}"#,
    )
    .expect("parses");
    let binds: Vec<(u16, Option<&str>)> = manifest
        .port_binds()
        .iter()
        .map(|grant| (grant.bind(), grant.address()))
        .collect();
    assert_eq!(binds, vec![(8443, Some("127.0.0.1")), (9090, None)]);
    assert_eq!(manifest.connect_local(), &[8443, 9443]);
    assert_eq!(manifest.dimensions().ports, manifest.port_binds());
    assert_eq!(manifest.dimensions().connect_local, &[8443, 9443]);
}

/// The builders accumulate, and refuse a second grant for the same socket.
#[test]
fn the_builders_add_grants_and_refuse_duplicates() {
    let manifest = ConfinementManifest::new()
        .with_port_bind(9090, None)
        .and_then(|m| m.with_port_bind(8443, None))
        .and_then(|m| m.with_connect_local(9443))
        .and_then(|m| m.with_connect_local(8443))
        .expect("distinct grants accumulate");
    assert_eq!(manifest.port_binds().len(), 2);
    assert_eq!(manifest.connect_local(), &[8443, 9443]);

    assert!(manifest.clone().with_port_bind(8443, None).is_err());
    assert!(
        manifest
            .clone()
            .with_port_bind(8443, Some("127.0.0.1"))
            .is_err(),
        "an explicit default address names the same socket as an omitted one"
    );
    assert!(manifest.clone().with_connect_local(8443).is_err());
    assert!(
        manifest.with_port_bind(8443, Some("0.0.0.0")).is_ok(),
        "the same port on a different address is a different socket"
    );
}
