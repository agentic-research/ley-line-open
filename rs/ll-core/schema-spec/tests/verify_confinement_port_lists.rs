//! `confinement/v1` §4 `port` and §3 `network.connectLocal` as lists.
//!
//! ## Why (bead `ley-line-open-e2c33f`)
//!
//! cloister ADR-0069 derives listener grants from a bundle's declared
//! `ports[]`, and real services bind more than one (grpc and metrics). Both
//! clauses were single-valued. The extension is additive within v1: a scalar
//! stays legal, a list is new, and the canonical form is chosen so that every
//! document valid before the extension digests exactly as it did.
//!
//! This test is the schema half of `test-vectors/manifest-port-list.json`. It
//! checks what the spec tree can check without an implementation: every
//! input and every canonical form satisfies the schema, the schema refuses
//! the documented rejects, every pinned `confinementDigest` is BLAKE3-256 of
//! its canonical bytes, and the one-element-list case is byte-identical to the
//! pinned `manifest-canonical.json`. The implementation half — that a parser
//! reaches those canonical bytes and computes the meet — is
//! `confinement_port_list_vectors` in `leyline-runtime`.

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
    let raw = std::fs::read_to_string(spec_dir().join("test-vectors/manifest-port-list.json"))
        .expect("the port-list vector file");
    serde_json::from_str(&raw).expect("vector file is valid JSON")
}

fn entries<'a>(vectors: &'a Value, key: &str) -> &'a [Value] {
    vectors[key]
        .as_array()
        .unwrap_or_else(|| panic!("vector file must carry a `{key}` array"))
}

fn digest_of(canonical: &str) -> String {
    format!("blake3-256:{}", blake3::hash(canonical.as_bytes()).to_hex())
}

fn assert_valid(validator: &Validator, what: &str, document: &Value) {
    if let Err(error) = validator.validate(document) {
        panic!("{what} does not satisfy confinement.schema.json: {error}\n{document:#}");
    }
}

/// Every pinned digest is the BLAKE3-256 of the canonical bytes beside it,
/// and every input and canonical form is a valid v1 document.
#[test]
fn every_case_and_meet_digests_to_its_pin_and_satisfies_the_schema() {
    let validator = schema();
    let vectors = vectors();
    let mut checked = 0usize;
    for (section, inputs) in [("cases", &["input"][..]), ("meets", &["left", "right"][..])] {
        for case in entries(&vectors, section) {
            let name = case["name"].as_str().expect("every entry is named");
            for input in inputs {
                assert_valid(
                    &validator,
                    &format!("{section}/{name}/{input}"),
                    &case[*input],
                );
            }
            let canonical = case["canonical"].as_str().expect("canonical is a string");
            let parsed: Value = serde_json::from_str(canonical).expect("canonical is JSON");
            assert_valid(&validator, &format!("{section}/{name}/canonical"), &parsed);
            assert_eq!(
                case["confinementDigest"].as_str(),
                Some(digest_of(canonical).as_str()),
                "{section}/{name}: the pinned confinementDigest is not BLAKE3-256 of the \
                 canonical bytes beside it"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 13,
        "expected every case and meet to be checked, got {checked}"
    );
}

/// The digests-unchanged proof, at the spec level.
///
/// The pinned single-port vector and a one-element list of the same grant
/// both canonicalize to the bytes of `manifest-canonical.json`, so both reach
/// the digest `CONFINEMENT_DIGESTS.blake3` already pins. A canonical form that
/// emitted a one-element list as a list would move this digest, and with it
/// every committed v1 identity carrying a §4 block.
#[test]
fn a_one_element_list_reaches_the_pinned_single_port_digest() {
    const PINNED: &str = "d9b5b7270bb6e5ec068aec92798dd76b0f71d1fe2640b3a09833b7742d51c617";
    let pinned_bytes =
        std::fs::read_to_string(spec_dir().join("test-vectors/manifest-canonical.json"))
            .expect("the pinned canonical manifest");
    assert_eq!(
        blake3::hash(pinned_bytes.as_bytes()).to_hex().as_str(),
        PINNED,
        "manifest-canonical.json itself moved"
    );
    let vectors = vectors();
    for name in [
        "port-scalar-is-the-pinned-vector",
        "port-one-element-list-is-the-scalar",
    ] {
        let case = entries(&vectors, "cases")
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("case {name} is missing"));
        assert_eq!(
            case["canonical"].as_str(),
            Some(pinned_bytes.as_str()),
            "{name} must canonicalize to the pinned vector's exact bytes"
        );
        assert_eq!(
            case["confinementDigest"].as_str(),
            Some(format!("blake3-256:{PINNED}").as_str()),
            "{name} must reach the pinned single-port digest"
        );
    }
}

/// The schema refuses every reject it can express.
#[test]
fn the_schema_refuses_the_list_rejects_it_can_express() {
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
            // Expressible only by the parser; recorded so the gap is visible.
            Some(false) => assert!(
                validator.validate(input).is_ok(),
                "reject {name} is marked parser-only but the schema refuses it — \
                 update `schemaRefuses`"
            ),
            None => panic!("reject {name} must say whether the schema refuses it"),
        }
    }
    assert!(
        refused >= 6,
        "expected the schema to refuse at least six rejects"
    );
}
