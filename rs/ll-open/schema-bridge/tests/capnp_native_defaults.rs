//! capnp's native field defaults reach the generated output
//! (`ley-line-open-f72fca`, reported by cloister).
//!
//! ## The defect
//!
//! `ley-line-open-8c00c6` made the emitter fill every field with capnp's
//! IMPLICIT default so appending a field stays backwards-compatible. But it
//! used the **type's** zero value, and never the field's **declared** value —
//! because `inputs/capnp.rs` does not read `slot.defaultValue` at all. The IR's
//! `StructField.default` carries only the `$Default(json)` ANNOTATION.
//!
//! So a schema saying
//!
//! ```capnp
//! struct Flags { loud @0 :Bool = true; }
//! ```
//!
//! generated `loud: z.boolean().default(false)` — the exact opposite of what
//! the schema declares. Before 8c00c6 no defaults were emitted at all, so this
//! could not happen; the fix introduced it, and for any field with a declared
//! value it is strictly worse than the required-field behaviour it replaced.
//!
//! ## Why silence here is worse than elsewhere
//!
//! schema-bridge's whole discipline is refuse-rather-than-guess: an unknown
//! annotation is a hard error (`check_annotations`, whose comment says the list
//! "forces a decision (handle or remove)"), and an unmapped type is
//! `UnmappedConstruct`. Declared defaults were neither honoured NOR refused —
//! they were silently dropped. The rule covered types and annotations but not
//! slot metadata.
//!
//! That matters more for capnp than it would elsewhere: **capnp has no linter.**
//! Nothing else tells an author their `= true` is being ignored downstream, so
//! schema-bridge refusing IS the lint for these schemas.
//!
//! ## Precedence this file pins
//!
//! `$Default(json)` annotation  >  capnp `= value`  >  the type's implicit zero
//!
//! An explicit annotation overrides the schema; the schema overrides the type.

use capnp::message::{Builder, HeapAllocator};
use capnp::schema_capnp;

use leyline_schema_bridge::{inputs, outputs};

fn parse(message: &Builder<HeapAllocator>) -> leyline_schema_bridge::ir::Schema {
    let reader = message
        .get_root_as_reader::<schema_capnp::code_generator_request::Reader>()
        .expect("root");
    inputs::capnp::parse(reader).expect("parse")
}

fn fill_file_node(mut node: schema_capnp::node::Builder<'_>, id: u64, name: &str) {
    node.set_id(id);
    node.set_display_name(name);
    node.set_display_name_prefix_length(0);
    node.set_file(());
}

/// The rendered right-hand side for `name:` in the emitted zod object.
fn rendered<'a>(zod: &'a str, name: &str) -> &'a str {
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

/// Build a one-struct schema whose fields carry explicit capnp defaults.
///
/// Every scalar kind, not just `Bool`. A test named for declared-default
/// handling that exercised only one type would pass while the rest stayed
/// broken — the `ley-line-open-c3916b` lesson, applied here rather than
/// relearned.
fn schema_with_native_defaults() -> leyline_schema_bridge::ir::Schema {
    let mut message = Builder::new_default();
    {
        let request = message.init_root::<schema_capnp::code_generator_request::Builder>();
        let mut nodes = request.init_nodes(2);
        fill_file_node(nodes.reborrow().get(0), 0xFFFE, "test.capnp");

        let mut node = nodes.reborrow().get(1);
        node.set_id(0xAAAA);
        node.set_display_name("test.capnp:Defaults");
        node.set_display_name_prefix_length("test.capnp:".len() as u32);
        let mut s = node.init_struct();
        s.set_discriminant_count(0);
        let mut fields = s.init_fields(5);

        {
            let mut f = fields.reborrow().get(0);
            f.set_name("loud");
            f.set_code_order(0);
            let mut slot = f.init_slot();
            slot.reborrow().init_type().set_bool(());
            slot.reborrow().init_default_value().set_bool(true);
            slot.set_had_explicit_default(true);
        }
        {
            let mut f = fields.reborrow().get(1);
            f.set_name("label");
            f.set_code_order(1);
            let mut slot = f.init_slot();
            slot.reborrow().init_type().set_text(());
            slot.reborrow().init_default_value().set_text("hi");
            slot.set_had_explicit_default(true);
        }
        {
            let mut f = fields.reborrow().get(2);
            f.set_name("count");
            f.set_code_order(2);
            let mut slot = f.init_slot();
            slot.reborrow().init_type().set_int32(());
            slot.reborrow().init_default_value().set_int32(7);
            slot.set_had_explicit_default(true);
        }
        {
            let mut f = fields.reborrow().get(3);
            f.set_name("size");
            f.set_code_order(3);
            let mut slot = f.init_slot();
            slot.reborrow().init_type().set_uint16(());
            slot.reborrow().init_default_value().set_uint16(42);
            slot.set_had_explicit_default(true);
        }
        // The control: NO explicit default. Must still get the type's implicit
        // zero, so honouring declared defaults does not regress 8c00c6.
        {
            let mut f = fields.reborrow().get(4);
            f.set_name("plain");
            f.set_code_order(4);
            let mut slot = f.init_slot();
            slot.reborrow().init_type().set_text(());
        }
    }
    parse(&message)
}

#[test]
fn capnp_declared_defaults_reach_the_generated_zod() {
    let zod = outputs::zod::emit(&schema_with_native_defaults()).expect("emit");

    assert_eq!(
        rendered(&zod, "loud"),
        "z.boolean().default(true)",
        "`loud @0 :Bool = true` must emit .default(true). Emitting .default(false) \
         is the type's zero standing in for a value the schema declares — the \
         exact inversion of what the author wrote.\n\n{zod}",
    );
    assert_eq!(
        rendered(&zod, "label"),
        r#"z.string().default("hi")"#,
        "declared Text default must survive\n\n{zod}",
    );
    assert_eq!(
        rendered(&zod, "count"),
        "z.number().int().default(7)",
        "declared Int32 default must survive\n\n{zod}",
    );
    assert_eq!(
        rendered(&zod, "size"),
        "z.number().int().nonnegative().default(42)",
        "declared UInt16 default must survive\n\n{zod}",
    );
}

#[test]
fn a_field_without_a_declared_default_still_gets_the_implicit_one() {
    // Guards the 8c00c6 property while fixing f72fca: absent must still mean
    // the type's zero, or append-only compatibility breaks again.
    let zod = outputs::zod::emit(&schema_with_native_defaults()).expect("emit");
    assert_eq!(
        rendered(&zod, "plain"),
        r#"z.string().default("")"#,
        "a field with NO declared default must keep capnp's implicit one\n\n{zod}",
    );
}

#[test]
fn declared_defaults_reach_the_json_schema_emitter_too() {
    // json_schema.rs honoured the $Default ANNOTATION but had the same blind
    // spot for native defaults, since both read the one IR field. Fixing only
    // zod would leave two generated artifacts disagreeing about the same
    // schema — the failure generating both halves exists to prevent.
    let doc = outputs::json_schema::emit(&schema_with_native_defaults(), "defaults")
        .expect("emit json schema");
    let parsed: serde_json::Value =
        serde_json::from_str(&doc).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{doc}"));
    let props = &parsed["$defs"]["Defaults"]["properties"];

    assert_eq!(
        props["loud"]["default"],
        serde_json::json!(true),
        "declared Bool default must reach JSON Schema\n\n{doc}",
    );
    assert_eq!(
        props["count"]["default"],
        serde_json::json!(7),
        "declared Int32 default must reach JSON Schema\n\n{doc}",
    );
}
