//! `$Traits.doc` must reach every target, not only the JSON ones.
//!
//! ## Why (bead `ley-line-open-d554a0`)
//!
//! The IR carries `doc: Option<String>` on structs, fields and enums, and the
//! JSON Schema and tool-definitions emitters both lower it. The zod and Go
//! emitters drop it silently.
//!
//! That omission is not cosmetic — it is what forces a consumer to keep a
//! second, hand-written copy of every generated type just to hold the
//! documentation. Cloister maintains `cluster-types.ts` (743 lines) beside the
//! generated `cluster.zod.ts` for exactly this reason, plus `types.ts` (441)
//! with no generated counterpart at all. A generator that drops the prose
//! guarantees a hand-maintained mirror, and a hand-maintained mirror is a
//! thing that silently disagrees.
//!
//! These tests pin the projection per target so a future emitter cannot quietly
//! stop carrying it.

use leyline_schema_bridge::ir::{Enum, FieldType, ScalarType, Schema, Struct, StructField};
use leyline_schema_bridge::outputs;

const STRUCT_DOC: &str = "A run's resolved authority, bound to one spec digest.";
const FIELD_DOC: &str = "Absolute expiry in Unix milliseconds.";
const ENUM_DOC: &str = "Isolation class required by resolved policy.";

fn documented_schema() -> Schema {
    Schema {
        enums: vec![Enum {
            name: "BackendClass".to_owned(),
            doc: Some(ENUM_DOC.to_owned()),
            variants: vec!["native".to_owned(), "microVm".to_owned()],
        }],
        structs: vec![Struct {
            name: "RunGrant".to_owned(),
            doc: Some(STRUCT_DOC.to_owned()),
            op: None,
            fields: vec![StructField {
                name: "expiresAtUnixMs".to_owned(),
                ordinal: 0,
                ty: FieldType::Scalar(ScalarType::UInt64),
                doc: Some(FIELD_DOC.to_owned()),
                optional: false,
                default: None,
            }],
            union: None,
        }],
        consts: Vec::new(),
    }
}

#[test]
fn zod_carries_struct_field_and_enum_docs() {
    let emitted = outputs::zod::emit(&documented_schema()).expect("emit zod");

    for (label, doc) in [
        ("struct", STRUCT_DOC),
        ("field", FIELD_DOC),
        ("enum", ENUM_DOC),
    ] {
        assert!(
            emitted.contains(doc),
            "zod output dropped the {label} doc comment.\n--- emitted ---\n{emitted}"
        );
    }
}

#[test]
fn go_carries_struct_field_and_enum_docs() {
    let emitted = outputs::go::emit(&documented_schema(), "execution").expect("emit go");

    for (label, doc) in [
        ("struct", STRUCT_DOC),
        ("field", FIELD_DOC),
        ("enum", ENUM_DOC),
    ] {
        assert!(
            emitted.contains(doc),
            "go output dropped the {label} doc comment.\n--- emitted ---\n{emitted}"
        );
    }
}

/// An unannotated schema must not gain empty comment blocks — the absence of
/// a doc is a valid state, not a hole to fill with `/** */`.
#[test]
fn an_undocumented_schema_emits_no_empty_comment() {
    let mut schema = documented_schema();
    schema.enums[0].doc = None;
    schema.structs[0].doc = None;
    schema.structs[0].fields[0].doc = None;

    let zod = outputs::zod::emit(&schema).expect("emit zod");
    assert!(!zod.contains("/**"), "empty TSDoc block emitted:\n{zod}");
}
