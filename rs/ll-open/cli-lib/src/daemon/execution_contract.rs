//! Shared execution/v1 request constructors for first-party Rust surfaces.
//!
//! The daemon, CLI client, and embedded callers must put the same operation
//! names and JSON envelope fields on the wire. Keeping these constructors in
//! one module removes a class of client/CLI drift while the generated
//! `execution-tools.json` remains the schema authority for nested payloads.

use serde_json::{Value, json};

pub const CAPABILITIES_OP: &str = "llo_execution_capabilities";
pub const STATUS_OP: &str = "llo_execution_status";
pub const PROVISION_OP: &str = "llo_execution_provision";
pub const START_OP: &str = "llo_execution_start";
pub const INSPECT_OP: &str = "llo_execution_inspect";
pub const CANCEL_OP: &str = "llo_execution_cancel";
pub const COLLECT_OP: &str = "llo_execution_collect";
pub const CLEANUP_OP: &str = "llo_execution_cleanup";

/// Generated tool order is stable and is used by MCP/server descriptors.
pub const OP_NAMES: [&str; 8] = [
    CAPABILITIES_OP,
    STATUS_OP,
    PROVISION_OP,
    START_OP,
    INSPECT_OP,
    CANCEL_OP,
    COLLECT_OP,
    CLEANUP_OP,
];

pub fn capabilities() -> Value {
    json!({"op": CAPABILITIES_OP})
}

pub fn provision(backend_class: &str, idempotency_key: &str) -> Value {
    json!({
        "op": PROVISION_OP,
        "backendClass": backend_class,
        "idempotencyKey": idempotency_key,
    })
}

pub fn status(run_id: Option<&str>) -> Value {
    json!({"op": STATUS_OP, "runId": run_id.unwrap_or("")})
}

pub fn start(spec: Value, grant: Value) -> Value {
    json!({"op": START_OP, "spec": spec, "grant": grant})
}

pub fn inspect(run_id: &str, after_sequence: u64) -> Value {
    json!({
        "op": INSPECT_OP,
        "runId": run_id,
        "afterSequence": after_sequence,
    })
}

pub fn cancel(run_id: &str, idempotency_key: Option<&str>) -> Value {
    json!({
        "op": CANCEL_OP,
        "runId": run_id,
        "idempotencyKey": idempotency_key.unwrap_or(""),
    })
}

pub fn collect(run_id: &str) -> Value {
    json!({"op": COLLECT_OP, "runId": run_id})
}

pub fn cleanup(run_id: &str, idempotency_key: Option<&str>) -> Value {
    json!({
        "op": CLEANUP_OP,
        "runId": run_id,
        "idempotencyKey": idempotency_key.unwrap_or(""),
    })
}

#[cfg(test)]
mod tests {
    use super::OP_NAMES;
    use serde_json::Value;

    #[test]
    fn operation_names_match_generated_mcp_tooldefs() {
        let tools: Vec<Value> = serde_json::from_str(include_str!("execution-tools.json"))
            .expect("generated execution tooldefs must be valid JSON");
        let generated: Vec<&str> = tools
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .expect("generated execution tool must have a name")
            })
            .collect();
        assert_eq!(generated, OP_NAMES);
    }
}
