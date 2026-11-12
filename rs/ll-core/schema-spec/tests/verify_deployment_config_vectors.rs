//! `deployment-config/v1` vectors (ADR-0049, bead `ley-line-open-52c177`).
//!
//! Three oracles meet here:
//! - the pins: each case's canonical resolved bytes and `configDigest`, and
//!   each reject's field and reason;
//! - the Rust resolver, `leyline_schema_spec::deployment_config`;
//! - the published JSON Schemas, which must accept every accepted input and
//!   refuse exactly the rejects marked `schemaRefuses`;
//! - the Python reference, `ref-impl-py/deployment_config_v1.py`, standard
//!   library only, which reaches every canonical byte string and digest from
//!   the resolved documents with its own reader, writer and BLAKE3.
//!
//! `configDigest` names one resolved configuration. It is not an authority
//! and is never resolved against a CAS.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use jsonschema::Validator;
use leyline_schema_spec::deployment_config::{
    self, HostFacts, Kind, Os, Reason, Syntax, published_schema,
};
use serde_json::Value;

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deployment-config/v1")
}

fn vectors(name: &str) -> Value {
    let raw = fs::read_to_string(spec_dir().join("test-vectors").join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn host(file: &Value, name: &str) -> HostFacts {
    let h = &file["hosts"][name];
    assert!(h.is_object(), "unknown host {name}");
    let opt = |k: &str| h[k].as_str().map(Into::into);
    HostFacts {
        home: opt("home"),
        xdg_state_home: opt("xdgStateHome"),
        data_dir: opt("dataDir"),
        os: match h["os"].as_str().unwrap() {
            "macos" => Os::MacOs,
            "linux" => Os::Linux,
            other => panic!("unknown os {other}"),
        },
    }
}

fn kind_of(name: &str) -> Kind {
    Kind::from_name(name).unwrap_or_else(|| panic!("unknown kind {name}"))
}

fn syntax(case: &Value) -> Syntax {
    match case["syntax"].as_str().unwrap_or("json") {
        "json" => Syntax::Json,
        "toml" => Syntax::Toml,
        other => panic!("unknown syntax {other}"),
    }
}

fn check_accept(file: &Value, kind: Kind, case: &Value, text: &str, syntax: Syntax) {
    let name = case["name"].as_str().unwrap();
    let host = host(file, case["host"].as_str().unwrap());
    let resolved = deployment_config::resolve(text.as_bytes(), syntax, &[kind], &host)
        .unwrap_or_else(|e| panic!("{name}: refused: {e}"));
    assert_eq!(
        String::from_utf8(resolved.canonical.clone()).unwrap(),
        case["canonical"].as_str().unwrap(),
        "{name}: canonical bytes"
    );
    assert_eq!(
        resolved.config_digest(),
        case["configDigest"].as_str().unwrap(),
        "{name}: configDigest"
    );
    // The canonical bytes are a fixpoint: resolving them again changes nothing.
    let again = deployment_config::resolve(&resolved.canonical, Syntax::Json, &[kind], &host)
        .unwrap_or_else(|e| panic!("{name}: canonical refused: {e}"));
    assert_eq!(again, resolved, "{name}: canonical is a fixpoint");
}

fn check_reject(file: &Value, kind: Kind, case: &Value, text: &str, syntax: Syntax) {
    let name = case["name"].as_str().unwrap();
    let host = host(file, case["host"].as_str().unwrap());
    let err = deployment_config::resolve(text.as_bytes(), syntax, &[kind], &host)
        .err()
        .unwrap_or_else(|| panic!("{name}: accepted"));
    let reason = Reason::from_code(case["reason"].as_str().unwrap())
        .unwrap_or_else(|| panic!("{name}: unknown reason {}", case["reason"]));
    assert_eq!(
        (err.field.as_str(), err.reason),
        (case["field"].as_str().unwrap(), reason),
        "{name}: {err}"
    );
}

const KIND_FILES: &[&str] = &[
    "daemon-config-cases.json",
    "execution-daemon-config-cases.json",
    "sign-helper-config-cases.json",
];

#[test]
fn every_kind_vector_resolves_or_refuses_as_pinned() {
    for file_name in KIND_FILES {
        let file = vectors(file_name);
        assert_eq!(file["apiVersion"], deployment_config::API_VERSION);
        let kind = kind_of(file["kind"].as_str().unwrap());
        let cases = file["cases"].as_array().unwrap();
        let rejects = file["rejects"].as_array().unwrap();
        assert!(!cases.is_empty() && !rejects.is_empty(), "{file_name}");
        for case in cases {
            check_accept(
                &file,
                kind,
                case,
                case["input"].as_str().unwrap(),
                syntax(case),
            );
        }
        for case in rejects {
            check_reject(
                &file,
                kind,
                case,
                case["input"].as_str().unwrap(),
                syntax(case),
            );
        }
    }
}

#[test]
fn canonical_strings_hold() {
    let file = vectors("canonical-strings.json");
    for case in file["cases"].as_array().unwrap() {
        let kind = kind_of(case["kind"].as_str().unwrap());
        check_accept(
            &file,
            kind,
            case,
            case["input"].as_str().unwrap(),
            Syntax::Json,
        );
    }
    for case in file["rejects"].as_array().unwrap() {
        let kind = kind_of(case["kind"].as_str().unwrap());
        check_reject(
            &file,
            kind,
            case,
            case["input"].as_str().unwrap(),
            Syntax::Json,
        );
    }
    // The pinned list tells UTF-8 order from UTF-16 order: U+FF61 sorts
    // before U+1F600 here, where JavaScript's default sort puts it after.
    let canonical = file["cases"][0]["canonical"].as_str().unwrap();
    let ff61 = canonical.find('\u{ff61}').unwrap();
    let grin = canonical.find('\u{1f600}').unwrap();
    assert!(ff61 < grin);
}

#[test]
fn toml_and_json_twins_resolve_to_the_same_bytes() {
    let file = vectors("toml-equivalence.json");
    for case in file["cases"].as_array().unwrap() {
        let kind = kind_of(case["kind"].as_str().unwrap());
        check_accept(
            &file,
            kind,
            case,
            case["toml"].as_str().unwrap(),
            Syntax::Toml,
        );
        check_accept(
            &file,
            kind,
            case,
            case["json"].as_str().unwrap(),
            Syntax::Json,
        );
    }
    for case in file["rejects"].as_array().unwrap() {
        let kind = kind_of(case["kind"].as_str().unwrap());
        check_reject(
            &file,
            kind,
            case,
            case["toml"].as_str().unwrap(),
            Syntax::Toml,
        );
    }
}

fn validator(kind: Kind) -> Validator {
    let schema: Value = serde_json::from_str(published_schema(kind)).unwrap();
    Validator::new(&schema).expect("a valid JSON Schema")
}

/// Every accepted JSON input and canonical document satisfies the published
/// schema; every reject marked `schemaRefuses` fails it, and every other
/// reject (a rule the schema cannot state) passes it, so the flag cannot go
/// stale in either direction.
#[test]
fn the_published_schemas_agree_with_the_vectors() {
    for file_name in KIND_FILES {
        let file = vectors(file_name);
        let kind = kind_of(file["kind"].as_str().unwrap());
        let validator = validator(kind);
        for case in file["cases"].as_array().unwrap() {
            for key in ["input", "canonical"] {
                let doc: Value = serde_json::from_str(case[key].as_str().unwrap()).unwrap();
                if let Err(e) = validator.validate(&doc) {
                    panic!("{file_name} {} ({key}): {e}", case["name"]);
                }
            }
        }
        for case in file["rejects"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let refuses = case["schemaRefuses"]
                .as_bool()
                .unwrap_or_else(|| panic!("{name} has no schemaRefuses"));
            let Ok(doc) = serde_json::from_str::<Value>(case["input"].as_str().unwrap()) else {
                assert!(
                    !refuses,
                    "{name} is not JSON but claims the schema refuses it"
                );
                continue;
            };
            let valid = validator.validate(&doc).is_ok();
            assert_eq!(
                !valid, refuses,
                "{file_name} {name}: schemaRefuses is {refuses}"
            );
        }
    }
}

/// The `schemaRefuses` flags of `canonical-strings.json`'s rejects hold
/// against their kind's published schema too.
#[test]
fn canonical_string_rejects_agree_with_the_schema() {
    let file = vectors("canonical-strings.json");
    for case in file["rejects"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let validator = validator(kind_of(case["kind"].as_str().unwrap()));
        let refuses = case["schemaRefuses"].as_bool().unwrap();
        let doc: Value = serde_json::from_str(case["input"].as_str().unwrap()).unwrap();
        assert_eq!(
            validator.validate(&doc).is_err(),
            refuses,
            "canonical-strings {name}: schemaRefuses is {refuses}"
        );
    }
}

/// The resolved document of the accept case of `file` with the most fields
/// set: the base the sweeps below mutate.
fn richest_case(file: &Value) -> (Value, String) {
    let case = file["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["syntax"].as_str().unwrap_or("json") == "json")
        .max_by_key(|c| c["input"].as_str().unwrap().len())
        .unwrap();
    (
        // The resolved document: every section present, and itself a valid
        // input (canonical bytes are a fixpoint).
        serde_json::from_str(case["canonical"].as_str().unwrap()).unwrap(),
        case["host"].as_str().unwrap().to_owned(),
    )
}

/// Every object path in `v`, dotted from the root ("" for the root), except
/// the label-keyed `signAllow.callers` map, whose keys are data.
fn object_paths(v: &Value, path: &str, out: &mut Vec<String>) {
    if let Value::Object(m) = v {
        if path != "signAllow.callers" {
            out.push(path.to_owned());
        }
        for (k, child) in m {
            let p = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            object_paths(child, &p, out);
        }
    }
}

fn at_mut<'v>(v: &'v mut Value, path: &str) -> &'v mut Value {
    if path.is_empty() {
        return v;
    }
    path.split('.').fold(v, |v, k| &mut v[k])
}

fn refusal_of(kind: Kind, host: &HostFacts, doc: &Value) -> (String, Reason) {
    let text = serde_json::to_vec(doc).unwrap();
    let e = deployment_config::resolve(&text, Syntax::Json, &[kind], host)
        .err()
        .unwrap_or_else(|| panic!("accepted {doc}"));
    (e.field, e.reason)
}

/// ADR-0049 test plan 2: an undefined key is refused in every object of
/// every kind — the root, every section, every union and union branch, and
/// every secret reference — naming its full dotted field.
#[test]
fn an_unknown_key_is_refused_in_every_object() {
    let mut swept = 0;
    for file_name in KIND_FILES {
        let file = vectors(file_name);
        let kind = kind_of(file["kind"].as_str().unwrap());
        let (base, host_name) = richest_case(&file);
        let host = host(&file, &host_name);
        let mut paths = Vec::new();
        object_paths(&base, "", &mut paths);
        assert!(paths.len() > 5, "{file_name}: {paths:?}");
        for path in paths {
            let mut doc = base.clone();
            at_mut(&mut doc, &path)
                .as_object_mut()
                .unwrap()
                .insert("zzUnknown".into(), Value::Bool(true));
            let want = if path.is_empty() {
                "zzUnknown".to_owned()
            } else {
                format!("{path}.zzUnknown")
            };
            assert_eq!(
                refusal_of(kind, &host, &doc),
                (want, Reason::UnknownKey),
                "{file_name}: an unknown key in {path:?}"
            );
            swept += 1;
        }
    }
    // Every object of the three resolved documents: 35 today. A smaller
    // count means a section stopped being swept.
    assert!(swept >= 35, "only {swept} objects swept");
}

/// ADR-0049 test plan 7 and 9a: in every secret field of every kind, a
/// string, a number, an array and `{"value": …}` are `secret-inlined`;
/// `keychain` and `vault` are `secret-ref-not-delivered`; any other key is
/// `unknown-key`.
#[test]
fn every_secret_field_refuses_every_inline_form() {
    let secret_fields: &[(&str, &[&str])] = &[
        (
            "daemon-config-cases.json",
            &["head.signingKey", "mcp.tokenFile"],
        ),
        ("execution-daemon-config-cases.json", &["head.signingKey"]),
        ("sign-helper-config-cases.json", &["auth.callerTokens"]),
    ];
    for (file_name, fields) in secret_fields {
        let file = vectors(file_name);
        let kind = kind_of(file["kind"].as_str().unwrap());
        let (base, host_name) = richest_case(&file);
        let host = host(&file, &host_name);
        for field in *fields {
            let (parent, leaf) = field.rsplit_once('.').unwrap();
            for (value, want_field, want) in [
                (
                    serde_json::json!("0303"),
                    field.to_string(),
                    Reason::SecretInlined,
                ),
                (
                    serde_json::json!(7),
                    field.to_string(),
                    Reason::SecretInlined,
                ),
                (
                    serde_json::json!(true),
                    field.to_string(),
                    Reason::SecretInlined,
                ),
                (
                    serde_json::json!(["/k"]),
                    field.to_string(),
                    Reason::SecretInlined,
                ),
                (
                    serde_json::json!({"value": "0303"}),
                    field.to_string(),
                    Reason::SecretInlined,
                ),
                (
                    serde_json::json!({"keychain": "x"}),
                    field.to_string(),
                    Reason::SecretRefNotDelivered,
                ),
                (
                    serde_json::json!({"vault": {"path": "kv/x"}}),
                    field.to_string(),
                    Reason::SecretRefNotDelivered,
                ),
                (
                    serde_json::json!({"other": "x"}),
                    format!("{field}.other"),
                    Reason::UnknownKey,
                ),
            ] {
                let mut doc = base.clone();
                let section = at_mut(&mut doc, parent);
                if !section.is_object() {
                    *section = serde_json::json!({});
                }
                section
                    .as_object_mut()
                    .unwrap()
                    .insert(leaf.into(), value.clone());
                assert_eq!(
                    refusal_of(kind, &host, &doc),
                    (want_field, want),
                    "{file_name}: {field} = {value}"
                );
            }
        }
    }
}

/// `verify_deployment_config_digest` (ADR-0049 D2): the independent Python
/// reference reaches every pinned canonical byte string and `configDigest`,
/// from the canonical bytes and from a shuffled resolved document per case,
/// and refuses the resolved rejects.
#[test]
fn verify_deployment_config_digest() {
    let output = Command::new("python3")
        .arg("-I")
        .arg(spec_dir().join("ref-impl-py/deployment_config_v1.py"))
        .arg("conformance")
        .arg(spec_dir())
        .output()
        .expect("python3 runs (the independent reference is part of the gate)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Python reference disagrees:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(", 0 failed"), "{stdout}");
}

/// The standalone example (ADR-0049 D8): the TOML and its JSON twin both
/// resolve, for HOME=/Users/fixture on macOS, to the pinned resolved file.
#[test]
fn the_standalone_example_resolves_to_its_pinned_file() {
    let examples =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/examples/deployment-config");
    let pinned = fs::read(examples.join("daemon-mount-macos.resolved.json")).unwrap();
    let host = HostFacts {
        home: Some("/Users/fixture".into()),
        xdg_state_home: None,
        data_dir: Some("/Users/fixture/Library/Application Support".into()),
        os: Os::MacOs,
    };
    for name in ["daemon-mount-macos.toml", "daemon-mount-macos.json"] {
        let path = examples.join(name);
        let text = fs::read(&path).unwrap();
        let syntax = Syntax::for_path(&path).unwrap();
        let resolved = deployment_config::resolve(&text, syntax, &[Kind::Daemon], &host)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(resolved.canonical, pinned, "{name}");
    }
}

/// Every reason a document can be refused for has a vector naming it.
#[test]
fn every_document_reason_has_a_vector() {
    let mut seen = std::collections::BTreeSet::new();
    for file_name in KIND_FILES
        .iter()
        .chain(&["canonical-strings.json", "toml-equivalence.json"])
    {
        for case in vectors(file_name)["rejects"].as_array().unwrap() {
            seen.insert(case["reason"].as_str().unwrap().to_owned());
        }
    }
    // Reasons a resolution cannot produce: host and binary checks, the file
    // rules, the extension check, and two rules the binaries own.
    let not_resolution = [
        Reason::UnknownMcpGroup,
        Reason::ResolveAllowSigningKey,
        Reason::UnknownConfigSyntax,
        Reason::UnsupportedOs,
        Reason::Unreadable,
        Reason::SecretFilePermissions,
        Reason::SecretFileInvalid,
        Reason::ConfigFilePermissions,
        Reason::ConfigDigestMismatch,
        Reason::MountFeatureNotBuilt,
        Reason::FuseFeatureNotBuilt,
        Reason::HostCheckFailed,
    ];
    for reason in Reason::ALL {
        if not_resolution.contains(reason) {
            continue;
        }
        assert!(
            seen.contains(reason.code()),
            "no vector refuses with {reason}"
        );
    }
}
