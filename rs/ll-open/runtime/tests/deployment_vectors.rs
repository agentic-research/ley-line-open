//! ADR-0049 D13 (bead `ley-line-open-52c177`): every `ExecutionDaemonConfig`
//! accept vector, run through the public library entry
//! `resolve_execution_deployment`, yields the pinned resolved bytes,
//! `configDigest` and backend struct values. The execution daemon's
//! `--config` path calls the same function, so the daemon and an embedder
//! are tested by one test.

use std::path::{Path, PathBuf};
use std::time::Duration;

use leyline_runtime::deployment::{
    BackendDeployment, HostFacts, Os, REMOVED_VARIABLES_JSON, Syntax, check_removed_environment,
    resolve_execution_deployment,
};
use serde_json::Value;

fn vectors(name: &str) -> Value {
    let path = Path::new(leyline_schema_spec::SPEC_DIR)
        .join("deployment-config/v1/test-vectors")
        .join(name);
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

fn host(file: &Value, name: &str) -> HostFacts {
    let h = &file["hosts"][name];
    let opt = |k: &str| h[k].as_str().map(Into::into);
    HostFacts {
        home: opt("home"),
        xdg_state_home: opt("xdgStateHome"),
        data_dir: opt("dataDir"),
        os: if h["os"] == "macos" {
            Os::MacOs
        } else {
            Os::Linux
        },
    }
}

fn paths(v: &Value) -> Vec<PathBuf> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| PathBuf::from(p.as_str().unwrap()))
        .collect()
}

fn ms(v: &Value) -> Duration {
    Duration::from_millis(v.as_u64().unwrap())
}

#[test]
fn every_execution_vector_builds_the_pinned_backend() {
    let file = vectors("execution-daemon-config-cases.json");
    let cases = file["cases"].as_array().unwrap();
    assert!(cases.len() >= 8);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let host = host(&file, case["host"].as_str().unwrap());
        let d = resolve_execution_deployment(
            case["input"].as_str().unwrap().as_bytes(),
            Syntax::Json,
            &host,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            std::str::from_utf8(&d.resolved).unwrap(),
            case["canonical"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(
            d.config_digest_text(),
            case["configDigest"].as_str().unwrap(),
            "{name}"
        );
        let b = &case["backend"];
        match (&d.backend, b["kind"].as_str().unwrap()) {
            (
                BackendDeployment::Native {
                    config,
                    cgroup_root,
                },
                "native",
            ) => {
                assert_eq!(
                    config.worker,
                    PathBuf::from(b["worker"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(
                    config.cas_root,
                    PathBuf::from(b["casRoot"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(
                    config.ephemeral_root,
                    PathBuf::from(b["ephemeralRoot"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(config.runtime_files, paths(&b["runtimeFiles"]), "{name}");
                assert_eq!(config.ready_timeout, ms(&b["readyTimeoutMs"]), "{name}");
                assert_eq!(
                    cgroup_root.as_deref(),
                    b["cgroupRoot"].as_str().map(Path::new),
                    "{name}"
                );
            }
            (BackendDeployment::MicroVm { config, workspace }, "microVm") => {
                assert_eq!(
                    config.worker,
                    PathBuf::from(b["worker"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(
                    config.cas_root,
                    PathBuf::from(b["casRoot"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(
                    config.ephemeral_root,
                    PathBuf::from(b["ephemeralRoot"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(config.runtime_files, paths(&b["runtimeFiles"]), "{name}");
                assert_eq!(config.devices, paths(&b["devices"]), "{name}");
                assert_eq!(config.ready_timeout, ms(&b["readyTimeoutMs"]), "{name}");
                assert_eq!(
                    config.tsi_hijack_inet,
                    b["tsiHijackInet"].as_bool().unwrap(),
                    "{name}"
                );
                assert_eq!(workspace.settled, ms(&b["workspace"]["ttlMs"]), "{name}");
                assert_eq!(
                    workspace.changed,
                    ms(&b["workspace"]["changedTtlMs"]),
                    "{name}"
                );
                assert_eq!(
                    workspace.window,
                    ms(&b["workspace"]["changeWindowMs"]),
                    "{name}"
                );
            }
            (other, kind) => panic!("{name}: built {other:?}, pinned {kind}"),
        }
    }
}

#[test]
fn every_execution_reject_is_refused_by_the_library_entry() {
    let file = vectors("execution-daemon-config-cases.json");
    for case in file["rejects"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let host = host(&file, case["host"].as_str().unwrap());
        let e = resolve_execution_deployment(
            case["input"].as_str().unwrap().as_bytes(),
            Syntax::Json,
            &host,
        )
        .err()
        .unwrap_or_else(|| panic!("{name}: accepted"));
        assert_eq!(
            (e.field.as_str(), e.reason.code()),
            (
                case["field"].as_str().unwrap(),
                case["reason"].as_str().unwrap()
            ),
            "{name}"
        );
    }
}

#[test]
fn ready_timeout_defaults_to_sixty_seconds() {
    let file = vectors("execution-daemon-config-cases.json");
    let case = &file["cases"][0];
    let d = resolve_execution_deployment(
        case["input"].as_str().unwrap().as_bytes(),
        Syntax::Json,
        &host(&file, case["host"].as_str().unwrap()),
    )
    .unwrap();
    let BackendDeployment::Native { config, .. } = d.backend else {
        panic!("the minimal document is native")
    };
    assert_eq!(config.ready_timeout, Duration::from_secs(60));
}

/// The library's removed-variable list is the published file, and three
/// removed variables set are each named with their key.
#[test]
fn check_removed_environment_names_every_offender() {
    let published = std::fs::read_to_string(
        Path::new(leyline_schema_spec::SPEC_DIR)
            .join("deployment-config/v1/published/leyline-removed-variables.json"),
    )
    .unwrap();
    assert_eq!(REMOVED_VARIABLES_JSON, published);
    let err = check_removed_environment([
        ("LEYLINE_HEAD_SIGNING_KEY".into(), "00".into()),
        ("HOME".into(), "/h".into()),
        ("LLO_QUERY_MATCH_LIMIT".into(), "9".into()),
        ("LEYLINE_SIGN_CALLER_TOKENS".into(), "a=b".into()),
    ])
    .unwrap_err();
    let named: Vec<(&str, &str)> = err
        .offenders
        .iter()
        .map(|v| (v.name.as_str(), v.replacement.as_str()))
        .collect();
    assert_eq!(
        named,
        [
            ("LEYLINE_HEAD_SIGNING_KEY", "head.signingKey"),
            ("LEYLINE_SIGN_CALLER_TOKENS", "auth.callerTokens"),
            ("LLO_QUERY_MATCH_LIMIT", "parser.queryMatchLimit"),
        ]
    );
}
