//! Append-only compatibility of the generated zod validator
//! (`ley-line-open-8c00c6`, filed from cloister; ADR-0004's ordinal rule).
//!
//! ## Claim
//!
//! capnp guarantees that appending a field at a higher ordinal is a
//! backwards-compatible change: every capnp scalar has an *implicit default*
//! (`Text` → `""`, `Bool` → `false`, integers → `0`, enums → their zeroth
//! variant, lists → empty), so a value written before the field existed is
//! still a valid value of the new schema. cloister's own schema header states
//! the guarantee it depends on:
//!
//! > New fields/variants append at higher ordinals; never renumber existing
//! > tags. Treat this file as forwards/backwards compatible — consumer
//! > manifests built against an older cloister must still parse here.
//!
//! The zod emitter contradicted that. It rendered every field as unconditionally
//! required and every object as `.strict()`, so appending one field broke six
//! pre-existing cloister fixtures with `ZodError: expected "string"`. Those
//! fixtures stand in for older values — precisely the case the rule covers.
//!
//! ## Why this is testable here, without a TypeScript toolchain
//!
//! LLO owns the generator but runs no `tsc`, so "the emitted zod behaves
//! correctly" is not directly observable in this repo. What *is* observable is
//! that the emitters disagree about the same IR: `json_schema.rs` honours
//! `$Optional` (`filter(|f| !f.optional)`) and `$Default`, while `zod.rs`
//! dropped both on the floor. Two artifacts generated from one source that
//! disagree about which fields may be absent is the exact failure generation
//! exists to make unrepresentable, and it is checkable from Rust.
//!
//! ## What breaks these gates
//!
//! - A field whose type carries a capnp implicit default rendered without a
//!   zod `.default(…)`, so absence fails validation.
//! - `$Optional` or `$Default` honoured by the JSON Schema emitter but not by
//!   the zod emitter.
//! - A defaulted field emitted with a value that is not what capnp would
//!   yield (e.g. `z.string().default("unset")`).

use leyline_schema_bridge::ir::{Enum, FieldType, ScalarType, Schema, Struct, StructField};
use leyline_schema_bridge::outputs;
use proptest::prelude::*;

/// A field carrying no annotations — the common case, and the one that
/// append-only compatibility turns on.
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

fn struct_of(name: &str, fields: Vec<StructField>) -> Struct {
    Struct {
        name: name.to_owned(),
        doc: None,
        op: None,
        fields,
        union: None,
    }
}

fn schema_of(structs: Vec<Struct>, enums: Vec<Enum>) -> Schema {
    Schema {
        enums,
        structs,
        consts: Vec::new(),
    }
}

/// The rendered right-hand side for `name:` in the emitted zod object.
fn rendered_field<'a>(zod: &'a str, name: &str) -> &'a str {
    let needle = format!("{name}: ");
    let line = zod
        .lines()
        .find(|l| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("no field `{name}` in emitted zod:\n{zod}"));
    line.trim()
        .strip_prefix(&needle)
        .unwrap()
        .trim_end_matches(',')
}

#[test]
fn appended_text_field_accepts_absence() {
    // The ley-line-open-8c00c6 scenario made executable. `metaNamespace` is
    // appended at the highest ordinal per ADR-0004, so a value written against
    // the two-field version of this struct must still validate.
    let schema = schema_of(
        vec![struct_of(
            "GatewayMetadata",
            vec![
                field("name", 0, FieldType::Scalar(ScalarType::Text)),
                field("version", 1, FieldType::Scalar(ScalarType::Text)),
                field("metaNamespace", 2, FieldType::Scalar(ScalarType::Text)),
            ],
        )],
        vec![],
    );

    let zod = outputs::zod::emit(&schema).expect("emit");

    assert_eq!(
        rendered_field(&zod, "metaNamespace"),
        r#"z.string().default("")"#,
        "an appended Text field must accept absence and yield capnp's implicit \
         default (\"\"); emitting a bare z.string() makes it required and breaks \
         ADR-0004's append-only guarantee.\n\nfull output:\n{zod}",
    );
}

#[test]
fn every_scalar_with_an_implicit_default_accepts_absence() {
    // Not just Text. capnp gives Bool false, every integer and float 0, and
    // Data an empty buffer. Fixing only the field that happened to break
    // cloister would leave the same bug waiting behind the next append.
    let cases: &[(&str, ScalarType, &str)] = &[
        ("t", ScalarType::Text, r#"z.string().default("")"#),
        ("b", ScalarType::Bool, "z.boolean().default(false)"),
        ("i8", ScalarType::Int8, "z.number().int().default(0)"),
        ("i64", ScalarType::Int64, "z.number().int().default(0)"),
        (
            "u32",
            ScalarType::UInt32,
            "z.number().int().nonnegative().default(0)",
        ),
        ("f64", ScalarType::Float64, "z.number().default(0)"),
        (
            "d",
            ScalarType::Data,
            "z.instanceof(Uint8Array).default(new Uint8Array())",
        ),
    ];

    let fields = cases
        .iter()
        .enumerate()
        .map(|(i, (name, ty, _))| field(name, i as u16, FieldType::Scalar(*ty)))
        .collect();
    let zod = outputs::zod::emit(&schema_of(vec![struct_of("S", fields)], vec![])).expect("emit");

    for (name, _, expected) in cases {
        assert_eq!(
            rendered_field(&zod, name),
            *expected,
            "scalar field `{name}` must accept absence with capnp's implicit \
             default\n\nfull output:\n{zod}",
        );
    }
}

#[test]
fn list_and_enum_fields_accept_absence() {
    // A capnp list defaults to empty and an enum to its zeroth variant. Both
    // are implicit defaults in exactly the same sense as the scalars.
    let schema = schema_of(
        vec![struct_of(
            "S",
            vec![
                field(
                    "tags",
                    0,
                    FieldType::List(Box::new(FieldType::Scalar(ScalarType::Text))),
                ),
                field("tier", 1, FieldType::EnumRef("Tier".to_owned())),
            ],
        )],
        vec![Enum {
            name: "Tier".to_owned(),
            doc: None,
            variants: vec!["cluster".to_owned(), "hypervisor".to_owned()],
        }],
    );

    let zod = outputs::zod::emit(&schema).expect("emit");

    assert_eq!(
        rendered_field(&zod, "tags"),
        "z.array(z.string()).readonly().default([])",
        "a capnp list defaults to empty\n\nfull output:\n{zod}",
    );
    assert_eq!(
        rendered_field(&zod, "tier"),
        r#"TierSchema.default("cluster")"#,
        "a capnp enum defaults to its ZEROTH variant — the IR keeps variants \
         position-stable so index 0 is authoritative\n\nfull output:\n{zod}",
    );
}

#[test]
fn zod_honours_the_same_annotations_as_json_schema() {
    // `$Optional` and `$Default` are already lowered into the IR and already
    // honoured by json_schema.rs. zod.rs read neither, so one IR produced two
    // artifacts that disagreed about which fields may be absent.
    let mut opt = field("curated", 0, FieldType::Scalar(ScalarType::Text));
    opt.optional = true;
    let mut dflt = field("mode", 1, FieldType::Scalar(ScalarType::Text));
    dflt.default = Some("\"task\"".to_owned());

    let schema = schema_of(vec![struct_of("S", vec![opt, dflt])], vec![]);
    let zod = outputs::zod::emit(&schema).expect("emit");
    let json_schema = outputs::json_schema::emit(&schema, "s").expect("emit json schema");

    assert_eq!(
        rendered_field(&zod, "curated"),
        "z.string().optional()",
        "`$Optional` is dropped from the JSON Schema `required` array; zod must \
         agree that the field may be absent\n\nfull output:\n{zod}",
    );
    assert_eq!(
        rendered_field(&zod, "mode"),
        r#"z.string().default("task")"#,
        "`$Default` is emitted as the JSON Schema `default`; zod must apply the \
         same value\n\nfull output:\n{zod}",
    );

    // The agreement stated directly, so the two emitters cannot drift apart
    // silently the way they already had. Parsed rather than substring-matched:
    // `curated` is legitimately present as a PROPERTY key, and only its
    // absence from `required` is the claim.
    let doc: serde_json::Value = serde_json::from_str(&json_schema).unwrap_or_else(|e| {
        panic!("json_schema emitter produced invalid JSON: {e}\n{json_schema}")
    });
    // `$defs`, not `definitions` — json_schema.rs emits JSON Schema 2020-12.
    //
    // This lookup previously read `doc["definitions"]`, fell back to a
    // top-level `doc["required"]` that emit() never writes, and finished with
    // `.unwrap_or_default()`. All three resolved to Null/empty, so
    // `!required.contains("curated")` was trivially true and the cross-emitter
    // claim asserted NOTHING — three layers of failing open, in the file whose
    // own docstring says absence must read as stale rather than as current.
    //
    // Resolved strictly now: a missing path is a test failure, not an empty
    // list. An assertion that cannot find what it is asserting about has not
    // succeeded.
    let defs = doc
        .get("$defs")
        .unwrap_or_else(|| panic!("json_schema emitter wrote no $defs:\n{json_schema}"));
    let s = defs
        .get("S")
        .unwrap_or_else(|| panic!("no $defs entry for struct S:\n{json_schema}"));
    let required: Vec<&str> = s
        .get("required")
        .unwrap_or_else(|| panic!("struct S has no `required` array:\n{json_schema}"))
        .as_array()
        .unwrap_or_else(|| panic!("`required` is not an array:\n{json_schema}"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("non-string in `required`:\n{json_schema}"))
        })
        .collect();

    // The positive half: the check is looking at a populated array, so the
    // negative assertion below cannot pass by finding nothing.
    assert!(
        required.contains(&"mode"),
        "`mode` carries $Default but is still a required capnp field, so it \
         must appear in `required`. If it does not, this lookup is inspecting \
         the wrong thing and the assertion below proves nothing.\n\n\
         required = {required:?}\n{json_schema}",
    );
    assert!(
        !required.contains(&"curated"),
        "an $Optional field must not appear in the JSON Schema required array; \
         this test's premise is wrong if it does\n\nrequired = {required:?}\n{json_schema}",
    );
}

// ── Properties over arbitrary IR ──────────────────────────────────────────
//
// The goldens above pin the cases we thought to write down. The append-only
// bug was a case nobody had written down, so the goldens could not have
// caught it. These properties state the rule itself and let proptest search
// for a field-type combination that violates it.

/// Does the rendered zod fragment accept the key being absent?
///
/// Stated as an observable on the emitted text rather than by re-deriving the
/// expected value — a property that recomputes the implementation would pass
/// no matter what the implementation did.
fn accepts_absence(fragment: &str) -> bool {
    fragment.contains(".default(") || fragment.ends_with(".optional()")
}

fn arb_scalar() -> impl Strategy<Value = ScalarType> {
    prop_oneof![
        Just(ScalarType::Bool),
        Just(ScalarType::Int8),
        Just(ScalarType::Int32),
        Just(ScalarType::Int64),
        Just(ScalarType::UInt8),
        Just(ScalarType::UInt32),
        Just(ScalarType::Float32),
        Just(ScalarType::Float64),
        Just(ScalarType::Text),
        Just(ScalarType::Data),
        Just(ScalarType::Void),
    ]
}

fn arb_field_type() -> impl Strategy<Value = FieldType> {
    prop_oneof![
        arb_scalar().prop_map(FieldType::Scalar),
        arb_scalar().prop_map(|s| FieldType::List(Box::new(FieldType::Scalar(s)))),
        Just(FieldType::List(Box::new(FieldType::List(Box::new(
            FieldType::Scalar(ScalarType::Text)
        ))))),
        Just(FieldType::EnumRef("Tier".to_owned())),
        Just(FieldType::StructRef("Other".to_owned())),
    ]
}

/// A schema of one struct whose fields have generated types, plus the enum
/// and struct those types may reference.
fn schema_with(types: Vec<FieldType>, optional: bool) -> Schema {
    let fields = types
        .into_iter()
        .enumerate()
        .map(|(i, ty)| {
            let mut f = field(&format!("f{i}"), i as u16, ty);
            f.optional = optional;
            f
        })
        .collect();
    schema_of(
        vec![struct_of("S", fields), struct_of("Other", vec![])],
        vec![Enum {
            name: "Tier".to_owned(),
            doc: None,
            variants: vec!["cluster".to_owned(), "hypervisor".to_owned()],
        }],
    )
}

proptest! {
    /// The append-only rule, stated over every field-type combination.
    ///
    /// capnp permits absence for everything that has an implicit default —
    /// which is every type EXCEPT `Void` (nothing to default) and a struct
    /// reference (no sound eager TS literal; see `capnp_implicit_default`).
    /// Those two exceptions are the complete documented carve-out, so any
    /// other type failing to accept absence is a compatibility break.
    #[test]
    fn unannotated_fields_accept_absence_exactly_when_capnp_does(
        types in prop::collection::vec(arb_field_type(), 1..8)
    ) {
        let schema = schema_with(types.clone(), false);
        let zod = outputs::zod::emit(&schema).expect("emit");

        for (i, ty) in types.iter().enumerate() {
            let name = format!("f{i}");
            let rendered = rendered_field(&zod, &name);
            let capnp_permits_absence = !matches!(
                ty,
                FieldType::Scalar(ScalarType::Void) | FieldType::StructRef(_)
            );
            prop_assert_eq!(
                accepts_absence(rendered),
                capnp_permits_absence,
                "field `{}` of type {:?} rendered as `{}`; capnp permits \
                 absence = {}\n\nfull output:\n{}",
                name, ty, rendered, capnp_permits_absence, zod
            );
        }
    }

    /// `$Optional` means "absent is meaningful" and must win over the field's
    /// type — including for the two types that have no implicit default.
    #[test]
    fn optional_fields_always_accept_absence(
        types in prop::collection::vec(arb_field_type(), 1..8)
    ) {
        let schema = schema_with(types.clone(), true);
        let zod = outputs::zod::emit(&schema).expect("emit");

        for i in 0..types.len() {
            let name = format!("f{i}");
            let rendered = rendered_field(&zod, &name);
            prop_assert!(
                accepts_absence(rendered),
                "$Optional field `{}` rendered as `{}` — an annotation the \
                 JSON Schema emitter honours must not be dropped here\n\n{}",
                name, rendered, zod
            );
        }
    }
}
