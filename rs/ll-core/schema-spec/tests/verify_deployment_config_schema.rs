//! `deployment-config/v1` schemas (ADR-0049 D2, D9; bead
//! `ley-line-open-52c177`):
//!
//! - the published, self-contained schemas and `leyline-removed-variables.json`
//!   are exactly what the generator makes from the sources (drift gate;
//!   `task gen:deployment-config-schema` regenerates them by running this
//!   test with `LEYLINE_REGEN_DEPLOYMENT_CONFIG=1`);
//! - every default appears twice and agrees: the schema's `default` and the
//!   resolver's (`verify_deployment_config_defaults`);
//! - every optional field states a default;
//! - a fail-closed default never loosens within v1.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use leyline_schema_spec::deployment_config::{
    self, HostFacts, Kind, Os, REMOVED_VARIABLES_JSON, Syntax, published_schema,
};
use serde_json::{Map, Value, json};

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deployment-config/v1")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const COMMON_REF: &str = "common.schema.json#/$defs/";

/// Rewrite every `$ref` into `common.schema.json` to a local `#/$defs/`
/// reference, collecting the names used.
fn localize(v: &Value, used: &mut BTreeSet<String>) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                if k == "$ref"
                    && let Some(s) = x.as_str()
                {
                    let name = s
                        .strip_prefix(COMMON_REF)
                        .or_else(|| s.strip_prefix("#/$defs/"));
                    if let Some(name) = name {
                        used.insert(name.to_owned());
                        out.insert(k.clone(), Value::String(format!("#/$defs/{name}")));
                        continue;
                    }
                }
                out.insert(k.clone(), localize(x, used));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|x| localize(x, used)).collect()),
        other => other.clone(),
    }
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

/// The published file of `kind`: the kind's schema with the common
/// definitions it reaches inlined, keys sorted, two-space indentation.
fn generate_schema(kind: Kind) -> String {
    let dir = spec_dir();
    let common = read_json(&dir.join("common.schema.json"));
    let defs = common["$defs"].as_object().unwrap();
    let source = read_json(&dir.join(format!("{}.schema.json", kind.short_name())));
    let mut used = BTreeSet::new();
    let mut schema = localize(&source, &mut used);
    let mut inlined = Map::new();
    let mut pending: Vec<String> = used.into_iter().collect();
    while let Some(name) = pending.pop() {
        if inlined.contains_key(&name) {
            continue;
        }
        let mut more = BTreeSet::new();
        let def = localize(
            defs.get(&name).unwrap_or_else(|| panic!("no common def {name}")),
            &mut more,
        );
        inlined.insert(name, def);
        pending.extend(more.into_iter().filter(|n| !inlined.contains_key(n)));
    }
    let obj = schema.as_object_mut().unwrap();
    obj.insert("$defs".into(), Value::Object(inlined));
    obj.insert(
        "$id".into(),
        Value::String(format!(
            "https://github.com/agentic-research/ley-line-open/rs/ll-core/schema-spec/deployment-config/v1/published/leyline-{}.schema.json",
            kind.short_name()
        )),
    );
    pretty(&schema)
}

fn generate_removed_variables() -> String {
    let source = read_json(&spec_dir().join("removed-variables.source.json"));
    let mut vars = source["variables"].as_array().unwrap().clone();
    vars.sort_by(|a, b| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()));
    pretty(&json!({"apiVersion": source["apiVersion"], "variables": vars}))
}

fn published_path(name: &str) -> PathBuf {
    spec_dir().join("published").join(name)
}

#[test]
fn published_artifacts_are_generated_from_the_sources() {
    let regen = std::env::var_os("LEYLINE_REGEN_DEPLOYMENT_CONFIG").is_some();
    let mut outputs: Vec<(String, String)> = Kind::ALL
        .iter()
        .map(|k| (format!("leyline-{}.schema.json", k.short_name()), generate_schema(*k)))
        .collect();
    outputs.push(("leyline-removed-variables.json".into(), generate_removed_variables()));
    for (name, want) in outputs {
        let path = published_path(&name);
        if regen {
            fs::write(&path, &want).unwrap();
            continue;
        }
        let have = fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "{name} drifted from its sources; run `task gen:deployment-config-schema`"
        );
    }
}

#[test]
fn the_embedded_copies_are_the_published_files() {
    for kind in Kind::ALL {
        let file = fs::read_to_string(published_path(&format!(
            "leyline-{}.schema.json",
            kind.short_name()
        )))
        .unwrap();
        assert_eq!(published_schema(*kind), file);
    }
    assert_eq!(
        REMOVED_VARIABLES_JSON,
        fs::read_to_string(published_path("leyline-removed-variables.json")).unwrap()
    );
}

fn host() -> HostFacts {
    HostFacts {
        home: Some("/Users/fixture".into()),
        xdg_state_home: None,
        data_dir: Some("/Users/fixture/Library/Application Support".into()),
        os: Os::MacOs,
    }
}

/// Follow `$ref` (local, in the published schema).
fn deref<'a>(schema: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let name = r.strip_prefix("#/$defs/").unwrap();
            deref(schema, &schema["$defs"][name])
        }
        None => node,
    }
}

/// The object schema of `node`: itself, or the object branch of a
/// `oneOf: [null, object]`.
fn object_branch<'a>(schema: &'a Value, node: &'a Value) -> Option<&'a Value> {
    let node = deref(schema, node);
    if node.get("properties").is_some() {
        return Some(node);
    }
    node.get("oneOf")?
        .as_array()?
        .iter()
        .map(|b| deref(schema, b))
        .find(|b| b.get("properties").is_some())
}

/// Walk `resolved` beside `input` under `node`: wherever the input omits a
/// key whose schema states a scalar or null `default`, the resolver's value
/// is that default; a host default (`x-leyline-default`) is present and not
/// null. Returns the number of leaves compared.
fn compare_defaults(
    schema: &Value,
    node: &Value,
    input: Option<&Value>,
    resolved: &Value,
    path: &str,
) -> usize {
    let Some(node) = object_branch(schema, node) else {
        return 0;
    };
    let Some(props) = node["properties"].as_object() else {
        return 0;
    };
    let mut compared = 0;
    for (key, prop) in props {
        let field = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
        let given = input.and_then(|i| i.get(key));
        let value = &resolved[key];
        let prop_d = deref(schema, prop);
        if given.is_none() {
            let default = prop.get("default").or_else(|| prop_d.get("default"));
            match default {
                Some(d) if !d.is_object() => {
                    assert_eq!(value, d, "{field}: the resolver's default differs from the schema's");
                    compared += 1;
                }
                _ => {}
            }
            if prop.get("x-leyline-default").is_some() || prop_d.get("x-leyline-default").is_some() {
                assert!(!value.is_null(), "{field}: a host default resolved to null");
                compared += 1;
            }
        }
        if value.is_object() {
            compared += compare_defaults(schema, prop, given, value, &field);
        }
    }
    compared
}

fn resolve_value(kind: Kind, input: &Value) -> Value {
    let text = serde_json::to_vec(input).unwrap();
    let resolved = deployment_config::resolve(&text, Syntax::Json, &[kind], &host())
        .unwrap_or_else(|e| panic!("{input}: {e}"));
    serde_json::from_slice(&resolved.canonical).unwrap()
}

/// `verify_deployment_config_defaults`: the resolver and the schema agree on
/// every default, over documents that leave every optional field out
/// (including those inside optional objects and union branches).
#[test]
fn verify_deployment_config_defaults() {
    let api = deployment_config::API_VERSION;
    let documents = [
        (Kind::Daemon, json!({"apiVersion": api, "kind": "DaemonConfig"})),
        (
            Kind::Daemon,
            json!({"apiVersion": api, "kind": "DaemonConfig", "mount": {"path": "/m"},
                   "source": {"path": "/s"}, "mcp": {"http": {"port": 1}}}),
        ),
        (
            Kind::ExecutionDaemon,
            json!({"apiVersion": api, "kind": "ExecutionDaemonConfig",
                   "cas": {"directory": {"root": "/c", "runRoot": "/r"}},
                   "worker": {"path": "/w"}, "catalog": "/cat", "evidence": {"unverified": {}}}),
        ),
        (
            Kind::ExecutionDaemon,
            json!({"apiVersion": api, "kind": "ExecutionDaemonConfig", "cas": {"volume": {}},
                   "worker": {"path": "/w"}, "catalog": "/cat", "backend": {"microVm": {}},
                   "evidence": {"unverified": {}}, "source": {"path": "/s"}}),
        ),
        (
            Kind::SignHelper,
            json!({"apiVersion": api, "kind": "SignHelperConfig",
                   "auth": {"callerTokens": {"file": "/t"}},
                   "signAllow": {"callers": {"a": ["op://x"]}}}),
        ),
    ];
    let mut compared = 0;
    for (kind, input) in &documents {
        let schema: Value = serde_json::from_str(published_schema(*kind)).unwrap();
        let resolved = resolve_value(*kind, input);
        compared += compare_defaults(&schema, &schema, Some(input), &resolved, "");
    }
    assert!(compared > 60, "only {compared} defaults compared");
}

/// Every optional field states its default (D2): outside a union object, a
/// property not in `required` has `default` or `x-leyline-default`.
#[test]
fn every_optional_field_states_a_default() {
    fn walk(schema: &Value, node: &Value, path: &str, missing: &mut Vec<String>) {
        let node = deref(schema, node);
        for branch in node.get("oneOf").and_then(Value::as_array).into_iter().flatten() {
            walk(schema, branch, path, missing);
        }
        let Some(props) = node.get("properties").and_then(Value::as_object) else {
            return;
        };
        let union = node.get("maxProperties") == Some(&json!(1));
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for (key, prop) in props {
            let field = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
            let d = deref(schema, prop);
            let stated = ["default", "x-leyline-default"]
                .iter()
                .any(|k| prop.get(*k).is_some() || d.get(*k).is_some());
            if !union && !required.contains(&key.as_str()) && !stated {
                missing.push(field.clone());
            }
            walk(schema, prop, &field, missing);
        }
    }
    for kind in Kind::ALL {
        let schema: Value = serde_json::from_str(published_schema(*kind)).unwrap();
        let mut missing = Vec::new();
        walk(&schema, &schema, "", &mut missing);
        // `evidence.unverified` and `evidence.verified` sit in a union; the
        // branches carry no default by design.
        assert!(missing.is_empty(), "{}: no default stated for {missing:?}", kind.name());
    }
}

/// Every `x-leyline-fail-closed` field of `schema`, as `(dotted field,
/// default)`.
fn fail_closed_defaults(schema: &Value) -> Vec<(String, Value)> {
    fn walk(schema: &Value, node: &Value, path: &str, out: &mut Vec<(String, Value)>) {
        let node = deref(schema, node);
        for branch in node.get("oneOf").and_then(Value::as_array).into_iter().flatten() {
            walk(schema, branch, path, out);
        }
        for (key, prop) in node.get("properties").and_then(Value::as_object).into_iter().flatten() {
            let field = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
            if prop.get("x-leyline-fail-closed") == Some(&json!(true)) {
                out.push((field.clone(), prop["default"].clone()));
            }
            walk(schema, prop, &field, out);
        }
    }
    let mut out = Vec::new();
    walk(schema, schema, "", &mut out);
    out
}

/// D9: a fail-closed default may not change within one major version
/// without the baseline (`fail-closed-defaults.json`, the defaults as last
/// released) changing too, and may never move to its loose value: loosening
/// is `config.leyline/v2`. Returns each violation.
fn fail_closed_violations(baseline: &Value, kind: Kind, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let pinned = &baseline["kinds"][kind.name()];
    for (field, default) in fail_closed_defaults(schema) {
        let Some(entry) = pinned.get(&field) else {
            out.push(format!("{}: {field} is fail-closed but has no baseline", kind.name()));
            continue;
        };
        if entry["default"] != default {
            if default == entry["loose"] {
                out.push(format!(
                    "{}: {field} loosened from {} to {default} under {}; loosening a fail-closed default is a new apiVersion",
                    kind.name(),
                    entry["default"],
                    deployment_config::API_VERSION
                ));
            } else {
                out.push(format!(
                    "{}: {field} changed from {} to {default}; record the narrowing in the baseline",
                    kind.name(),
                    entry["default"]
                ));
            }
        }
    }
    out
}

#[test]
fn fail_closed_defaults_never_loosen_within_v1() {
    let baseline = read_json(&spec_dir().join("fail-closed-defaults.json"));
    assert_eq!(baseline["apiVersion"], deployment_config::API_VERSION);
    let mut total = 0;
    for kind in Kind::ALL {
        let schema: Value = serde_json::from_str(published_schema(*kind)).unwrap();
        let violations = fail_closed_violations(&baseline, *kind, &schema);
        assert!(violations.is_empty(), "{violations:#?}");
        total += fail_closed_defaults(&schema).len();
        let pinned = baseline["kinds"][kind.name()].as_object().unwrap();
        assert_eq!(pinned.len(), fail_closed_defaults(&schema).len(), "{}: stale baseline entry", kind.name());
    }
    assert_eq!(total, 5, "mcp.noAuth, mcp.http.allowPublic, backend.microVm.tsiHijackInet, auth.required, signAllow.required");

    // A fixture whose fail-closed default loosened fails the gate.
    let mut loosened: Value = serde_json::from_str(published_schema(Kind::Daemon)).unwrap();
    loosened["properties"]["mcp"]["properties"]["noAuth"]["default"] = json!(true);
    let violations = fail_closed_violations(&baseline, Kind::Daemon, &loosened);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("loosened"), "{violations:?}");
    let mut loosened: Value = serde_json::from_str(published_schema(Kind::SignHelper)).unwrap();
    loosened["properties"]["auth"]["properties"]["required"]["default"] = json!(false);
    assert_eq!(fail_closed_violations(&baseline, Kind::SignHelper, &loosened).len(), 1);
}
