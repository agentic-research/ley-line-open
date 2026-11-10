//! `confinement/v1` §6 `guestPort`: a socket grant presented to a guest
//! workload as TCP on the guest's loopback.
//!
//! ## Why (bead `ley-line-open-be3711`, ADR-0046 Q7)
//!
//! The microVM tier delivers a §6 grant as a vsock port, which an unmodified
//! workload (a database server, a gRPC client) cannot use. The guest-loopback
//! presentation needs one more fact per grant, the guest port, and the
//! decision is that the issuer chooses it and the digest commits to it. The
//! extension is additive within v1: a grant without `guestPort` keeps its one
//! spelling and its digest.
//!
//! This test is the schema half of `test-vectors/manifest-guest-port.json`:
//! every input and canonical form satisfies the schema, every pinned
//! `confinementDigest` is BLAKE3-256 of its canonical bytes, and the schema
//! refuses the rejects it can express. The implementation half is
//! `confinement_guest_port_vectors` in `leyline-runtime`.

use jsonschema::Validator;
use serde_json::Value;

fn spec_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("confinement/v1")
}

fn schema() -> Validator {
    let raw = std::fs::read_to_string(spec_dir().join("confinement.schema.json"))
        .expect("confinement/v1 ships confinement.schema.json");
    let value: Value = serde_json::from_str(&raw).expect("schema is valid JSON");
    Validator::new(&value).expect("schema is a valid JSON Schema")
}

fn vectors() -> Value {
    let raw = std::fs::read_to_string(spec_dir().join("test-vectors/manifest-guest-port.json"))
        .expect("the guest-port vector file");
    serde_json::from_str(&raw).expect("vector file is valid JSON")
}

fn entries<'a>(vectors: &'a Value, key: &str) -> &'a [Value] {
    vectors[key]
        .as_array()
        .unwrap_or_else(|| panic!("vector file must carry a `{key}` array"))
}

fn assert_valid(validator: &Validator, what: &str, document: &Value) {
    if let Err(error) = validator.validate(document) {
        panic!("{what} does not satisfy confinement.schema.json: {error}\n{document:#}");
    }
}

#[test]
fn every_case_digests_to_its_pin_and_satisfies_the_schema() {
    let validator = schema();
    let vectors = vectors();
    let cases = entries(&vectors, "cases");
    for case in cases {
        let name = case["name"].as_str().expect("every case is named");
        assert_valid(&validator, &format!("{name}/input"), &case["input"]);
        let canonical = case["canonical"].as_str().expect("canonical is a string");
        let parsed: Value = serde_json::from_str(canonical).expect("canonical is JSON");
        assert_valid(&validator, &format!("{name}/canonical"), &parsed);
        assert_eq!(
            case["confinementDigest"].as_str(),
            Some(format!("blake3-256:{}", blake3::hash(canonical.as_bytes()).to_hex()).as_str()),
            "{name}: the pinned confinementDigest is not BLAKE3-256 of the canonical \
             bytes beside it"
        );
    }
    assert!(cases.len() >= 4, "expected four cases, got {}", cases.len());
}

/// Additivity: a document without `guestPort` canonicalizes with no
/// `guestPort` key and no connect object, exactly as before the extension.
#[test]
fn a_document_without_a_guest_port_has_the_pre_extension_form() {
    let vectors = vectors();
    let case = entries(&vectors, "cases")
        .iter()
        .find(|case| case["name"] == "no-guest-port-is-unchanged")
        .expect("the unchanged case");
    let canonical = case["canonical"].as_str().expect("canonical");
    assert!(!canonical.contains("guestPort"), "{canonical}");
    assert!(!canonical.contains("\"connect\""), "{canonical}");
}

#[test]
fn the_schema_refuses_the_guest_port_rejects_it_can_express() {
    let validator = schema();
    let vectors = vectors();
    let mut refused = 0usize;
    for reject in entries(&vectors, "rejects") {
        let name = reject["name"].as_str().expect("every reject is named");
        let input = &reject["input"];
        match reject["schemaRefuses"].as_bool() {
            Some(true) => {
                assert!(
                    validator.validate(input).is_err(),
                    "schema accepted reject {name}\n{input:#}"
                );
                refused += 1;
            }
            Some(false) => assert!(
                validator.validate(input).is_ok(),
                "reject {name} is marked parser-only but the schema refuses it — \
                 update `schemaRefuses`"
            ),
            None => panic!("reject {name} must say whether the schema refuses it"),
        }
    }
    assert!(refused >= 6, "expected the schema to refuse six rejects, got {refused}");
}
