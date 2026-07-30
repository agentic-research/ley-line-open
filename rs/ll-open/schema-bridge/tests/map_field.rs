//! `$Map` — a string-keyed dictionary, emitted as a native map by every
//! target that has one.
//!
//! Capnp has no map type. A schema that needs one carries `List(Entry)` where
//! `Entry` is `{ key :Text, value :T }` and marks the field `$Map`. These tests
//! pin the emit shape for all four targets, because the whole point of the
//! annotation is that `resolution["git-url"]` stays a lookup instead of
//! becoming a linear scan over an array of pairs.
//!
//! Driven through the IR rather than through `capnp compile`, matching the
//! crate's stated approach: tests drive the library directly so coverage does
//! not depend on having the capnp CLI installed.

use leyline_schema_bridge::{FieldType, ScalarType, Schema, Struct, StructField, outputs};

fn field(name: &str, ordinal: u16, ty: FieldType) -> StructField {
    StructField {
        name: name.to_owned(),
        ordinal,
        ty,
        doc: None,
        optional: false,
        default: None,
    }
}

/// A struct with one `Record<string, string>` and one `Record<string, Struct>`,
/// plus an ordinary list — so a regression that turned every list into a map
/// (or vice versa) fails rather than passing on a one-sided fixture.
fn schema_with_maps() -> Schema {
    let mut schema = Schema::new();
    schema.structs.push(Struct {
        name: "Resolution".to_owned(),
        doc: None,
        op: None,
        fields: vec![
            field("confidence", 0, FieldType::Scalar(ScalarType::Text)),
            field("how", 1, FieldType::Scalar(ScalarType::Text)),
        ],
        union: None,
    });
    schema.structs.push(Struct {
        name: "SiteMap".to_owned(),
        doc: None,
        op: None,
        fields: vec![
            field(
                "edgeKinds",
                0,
                FieldType::Map(Box::new(FieldType::Scalar(ScalarType::Text))),
            ),
            field(
                "resolution",
                1,
                FieldType::Map(Box::new(FieldType::StructRef("Resolution".to_owned()))),
            ),
            field(
                "plainList",
                2,
                FieldType::List(Box::new(FieldType::Scalar(ScalarType::Text))),
            ),
        ],
        union: None,
    });
    schema
}

#[test]
fn zod_emits_a_record_not_an_array() {
    let out = outputs::zod::emit(&schema_with_maps()).expect("emit");
    assert!(
        out.contains("edgeKinds: z.record(z.string(), z.string()).readonly()"),
        "scalar-valued map should be z.record; got:\n{out}"
    );
    assert!(
        out.contains("resolution: z.record(z.string(), ResolutionSchema).readonly()"),
        "struct-valued map should reference the struct's schema; got:\n{out}"
    );
    // The control. If this became a record too, the annotation would be
    // doing nothing and the assertions above would still pass.
    assert!(
        out.contains("plainList: z.array(z.string()).readonly()"),
        "an unannotated list must stay an array; got:\n{out}"
    );
}

#[test]
fn typescript_interface_matches_the_zod_shape() {
    let out = outputs::zod::emit(&schema_with_maps()).expect("emit");
    // Both halves or neither: a `z.ZodType<SiteMap>` declaration fails to
    // typecheck if the schema says readonly and the interface does not.
    assert!(
        out.contains("edgeKinds: Readonly<Record<string, string>>"),
        "TS field should be a readonly record; got:\n{out}"
    );
    assert!(
        out.contains("resolution: Readonly<Record<string, Resolution>>"),
        "struct-valued map should name the struct type; got:\n{out}"
    );
}

#[test]
fn go_emits_a_native_map() {
    let out = outputs::go::emit(&schema_with_maps(), "sitemap").expect("emit");
    assert!(
        out.contains("map[string]string"),
        "scalar-valued map should be map[string]string; got:\n{out}"
    );
    assert!(
        out.contains("map[string]Resolution"),
        "struct-valued map should be map[string]Resolution; got:\n{out}"
    );
    assert!(
        out.contains("[]string"),
        "an unannotated list must stay a slice; got:\n{out}"
    );
}

#[test]
fn json_schema_emits_an_object_with_additional_properties() {
    let out = outputs::json_schema::emit(&schema_with_maps(), "site-map").expect("emit");
    assert!(
        out.contains(r#""type": "object", "additionalProperties": { "type": "string" }"#),
        "scalar-valued map should be an open object; got:\n{out}"
    );
    assert!(
        out.contains(r##""additionalProperties": { "$ref": "#/$defs/Resolution" }"##),
        "struct-valued map should $ref the struct; got:\n{out}"
    );
    assert!(
        out.contains(r#""type": "array", "items": { "type": "string" }"#),
        "an unannotated list must stay an array; got:\n{out}"
    );
}
