//! `connections/v1` ships a JSON Schema, and it gates (ADR-0046 Slice 1 S2,
//! bead `ley-line-open-e9d940`).
//!
//! The schema is the shape a consumer generates types from; the parser in
//! `leyline_schema_spec::connections` is the stricter side of the pair. Every
//! canonical document must satisfy the schema, and every reject vector marked
//! `schemaRefuses` must fail it. A reject marked `schemaRefuses: false` is a
//! rule JSON Schema cannot state (uniqueness across connections, a duplicate
//! key, `30080.0` read as an integer, an escape that decodes to an allowed
//! character); the parser alone refuses it, and this test pins that the
//! schema does accept it, so the flag cannot go stale in either direction.

use jsonschema::Validator;
use serde_json::Value;
use std::path::Path;

fn spec_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("connections/v1")
}

fn schema() -> Validator {
    let raw = std::fs::read_to_string(spec_dir().join("connections.schema.json"))
        .expect("connections/v1 ships connections.schema.json");
    let value: Value = serde_json::from_str(&raw).expect("schema is JSON");
    Validator::new(&value).expect("schema is a valid JSON Schema")
}

fn cases() -> Value {
    let raw = std::fs::read_to_string(spec_dir().join("test-vectors/connections-cases.json"))
        .expect("cases file");
    serde_json::from_str(&raw).expect("cases file is JSON")
}

#[test]
fn every_canonical_document_satisfies_the_schema() {
    let validator = schema();
    let cases = cases();
    for case in cases["cases"].as_array().unwrap() {
        for key in ["input", "canonical"] {
            let text = case[key].as_str().unwrap();
            let doc: Value = serde_json::from_str(text).unwrap();
            if let Err(e) = validator.validate(&doc) {
                panic!("case {} ({key}) does not satisfy the schema: {e}", case["name"]);
            }
        }
    }
}

#[test]
fn the_schema_refuses_what_it_can_state() {
    let validator = schema();
    let cases = cases();
    for case in cases["rejects"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let refuses = case["schemaRefuses"]
            .as_bool()
            .unwrap_or_else(|| panic!("reject {name} has no schemaRefuses"));
        let text = case["input"].as_str().unwrap();
        let Ok(doc) = serde_json::from_str::<Value>(text) else {
            // Not JSON that serde_json reads, so there is nothing to validate;
            // only a refusal the parser owns can be marked this way.
            assert!(!refuses, "reject {name} is not JSON but claims the schema refuses it");
            continue;
        };
        let valid = validator.validate(&doc).is_ok();
        assert_eq!(
            !valid,
            refuses,
            "reject {name}: schemaRefuses is {refuses} but the schema {} it",
            if valid { "accepts" } else { "refuses" }
        );
    }
}
