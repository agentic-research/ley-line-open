//! End-to-end conformance for the public execution/v1 IDL.
//!
//! Unlike the emitter unit tests, this compiles the real schema. That makes the
//! Cap'n Proto file—not a hand-built mirror—the input shared by Rust wire
//! generation, JSON Schema, and MCP tool definitions.

use std::path::{Path, PathBuf};
use std::process::Command;

use capnp::message::ReaderOptions;
use capnp::schema_capnp;
use leyline_schema_bridge::{OutputFormat, Schema, emit, inputs};
use serde_json::{Value, json};

fn execution_schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/execution/v1/execution.capnp")
}

fn compile_execution_schema() -> Schema {
    let path = execution_schema_path();
    let output = Command::new("capnp")
        .arg("compile")
        .arg("-o-")
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("run capnp for {}: {e}", path.display()));
    assert!(
        output.status.success(),
        "capnp compile failed for {}:\n{}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut bytes = output.stdout.as_slice();
    let message = capnp::serialize::read_message(&mut bytes, ReaderOptions::new())
        .expect("decode CodeGeneratorRequest");
    let request = message
        .get_root::<schema_capnp::code_generator_request::Reader<'_>>()
        .expect("read CodeGeneratorRequest");
    inputs::capnp::parse(request).expect("lower execution/v1 schema")
}

fn assert_conforms(instance: &Value, schema: &Value, root: &Value, path: &str) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = root
            .pointer(reference.strip_prefix('#').expect("local schema reference"))
            .unwrap_or_else(|| panic!("{path}: unresolved schema reference {reference}"));
        assert_conforms(instance, target, root, path);
        return;
    }

    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        assert!(
            values.contains(instance),
            "{path}: {instance} is not one of {values:?}"
        );
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = instance
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected object, got {instance}"));
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("object schema properties");
            for required in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = required.as_str().expect("required field name");
                assert!(object.contains_key(name), "{path}: missing required {name}");
            }
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                for name in object.keys() {
                    assert!(
                        properties.contains_key(name),
                        "{path}: undeclared property {name}"
                    );
                }
            }
            for (name, value) in object {
                if let Some(property_schema) = properties.get(name) {
                    assert_conforms(value, property_schema, root, &format!("{path}.{name}"));
                }
            }
        }
        Some("array") => {
            let array = instance
                .as_array()
                .unwrap_or_else(|| panic!("{path}: expected array, got {instance}"));
            let item_schema = schema.get("items").expect("array item schema");
            for (index, value) in array.iter().enumerate() {
                assert_conforms(value, item_schema, root, &format!("{path}[{index}]"));
            }
        }
        Some("string") => assert!(instance.is_string(), "{path}: expected string"),
        Some("integer") => assert!(
            instance.as_i64().is_some() || instance.as_u64().is_some(),
            "{path}: expected integer"
        ),
        Some("number") => assert!(instance.is_number(), "{path}: expected number"),
        Some("boolean") => assert!(instance.is_boolean(), "{path}: expected boolean"),
        Some("null") => assert!(instance.is_null(), "{path}: expected null"),
        Some(other) => panic!("{path}: unsupported test schema type {other}"),
        None => {}
    }
}

#[test]
fn real_execution_schema_emits_public_types_and_operations() {
    let schema = compile_execution_schema();
    let struct_names: Vec<_> = schema.structs.iter().map(|s| s.name.as_str()).collect();
    for required in [
        "RunSpec",
        "RunGrant",
        "RunEvent",
        "RunReceipt",
        "ExecutionError",
        "StatusInput",
        "StatusOutput",
        "StartInput",
        "StartOutput",
    ] {
        assert!(
            struct_names.contains(&required),
            "execution/v1 is missing {required}; found {struct_names:?}"
        );
    }

    let json_schema =
        emit(&schema, OutputFormat::JsonSchema, "execution").expect("emit execution JSON Schema");
    let json_doc: Value = serde_json::from_str(&json_schema).expect("valid JSON Schema");
    assert_eq!(json_doc["$defs"]["RunSpec"]["type"], json!("object"));
    assert_eq!(json_doc["$defs"]["RunGrant"]["type"], json!("object"));
    assert_eq!(json_doc["$defs"]["RunReceipt"]["type"], json!("object"));

    let tool_defs =
        emit(&schema, OutputFormat::ToolDefs, "execution").expect("emit execution MCP tools");
    let tools: Value = serde_json::from_str(&tool_defs).expect("valid MCP tool definitions");
    let names: Vec<_> = tools
        .as_array()
        .expect("tool definitions array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(
        names,
        [
            "llo_execution_capabilities",
            "llo_execution_status",
            "llo_execution_provision",
            "llo_execution_start",
            "llo_execution_inspect",
            "llo_execution_cancel",
            "llo_execution_collect",
            "llo_execution_cleanup",
        ]
    );

    // A generated MCP tool must be portable by itself. StartInput contains
    // `$ref`s to RunSpec/RunGrant, so copying definitions by hand in Cloister
    // would defeat the IDL. The tool emitter carries the referenced schema
    // graph alongside the root input object.
    let start = tools
        .as_array()
        .expect("tool definitions array")
        .iter()
        .find(|tool| tool["name"] == "llo_execution_start")
        .expect("start tool");
    let defs = &start["inputSchema"]["$defs"];
    for required in [
        "RunSpec",
        "RunGrant",
        "ArtifactRef",
        "DigestRef",
        "EvidenceRef",
    ] {
        assert!(
            defs.get(required).is_some(),
            "start inputSchema has a dangling reference to {required}: {start}"
        );
    }
}

#[test]
fn intent_schema_cannot_name_raw_host_storage() {
    let schema = compile_execution_schema();
    let run_spec = schema
        .structs
        .iter()
        .find(|s| s.name == "RunSpec")
        .expect("RunSpec");
    let field_names: Vec<_> = run_spec.fields.iter().map(|f| f.name.as_str()).collect();

    for forbidden in ["hostPath", "arenaPath", "sqlitePath", "credentialValue"] {
        assert!(
            !field_names.contains(&forbidden),
            "RunSpec intent must not carry ambient authority field {forbidden}"
        );
    }
}

#[test]
fn canonical_carrier_vector_conforms_to_generated_schema() {
    let schema = compile_execution_schema();
    let emitted =
        emit(&schema, OutputFormat::JsonSchema, "execution").expect("emit execution JSON Schema");
    let json_schema: Value = serde_json::from_str(&emitted).expect("valid JSON Schema");
    let vector_path = execution_schema_path()
        .parent()
        .expect("execution/v1 parent")
        .join("test-vectors/canonical-run.json");
    let vector: Value = serde_json::from_slice(
        &std::fs::read(&vector_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", vector_path.display())),
    )
    .expect("parse canonical execution vector");

    for (field, definition) in [
        ("spec", "RunSpec"),
        ("grant", "RunGrant"),
        ("receipt", "RunReceipt"),
    ] {
        assert_conforms(
            &vector[field],
            &json_schema["$defs"][definition],
            &json_schema,
            field,
        );
    }
}
