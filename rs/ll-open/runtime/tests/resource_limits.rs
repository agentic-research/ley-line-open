//! OCI-aligned resource ceilings on execution/v1, per tier (bead
//! `ley-line-open-da36d2`, ADR-0035 "OCI resources on execution/v1").
//!
//! Every ceiling is either enforced by a named mechanism on the selected tier
//! or refused by name before anything runs (ADR-0035 §4). These tests pin both
//! halves for every field on every tier, the conformance vector a second
//! implementation runs, and the two ways the old shape could have slipped
//! through: a native backend that accepted memory it never applied, and a JSON
//! carrier that drops keys it does not know.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, CatalogResolver, CeilingMechanism,
    CpuLimits, DigestRef, EnforcedCeilings, ErrorCode, ExecutionError, ExecutionRequest,
    ExecutionService, MemoryLimits, MetadataOnlyEvidenceVerifier, PidsLimits, ResourceLimits,
    authorization::AuthorizationPolicy, transport,
};
use serde_json::{Value, json};

const MIB: u64 = 1024 * 1024;

/// The three tiers LLO ships, as each backend declares them.
fn microvm() -> EnforcedCeilings {
    EnforcedCeilings::hypervisor_backed()
}
fn native_with_cgroup() -> EnforcedCeilings {
    EnforcedCeilings {
        wall_time: CeilingMechanism::Supervisor,
        output_bytes: CeilingMechanism::Unenforced,
        memory: CeilingMechanism::Cgroup,
        cpu: CeilingMechanism::Cgroup,
        pids: CeilingMechanism::Cgroup,
    }
}
fn native_without_cgroup() -> EnforcedCeilings {
    EnforcedCeilings::wall_clock_only()
}

/// A request for exactly one ceiling.
fn only(field: &str) -> ResourceLimits {
    let mut limits = ResourceLimits {
        wall_time_ms: 1_000,
        ..ResourceLimits::default()
    };
    match field {
        "wallTimeMs" => {}
        "outputBytes" => limits.output_bytes = 4096,
        "memory.limit" => limits.memory = MemoryLimits { limit: 256 * MIB },
        "cpu.quota" => {
            limits.cpu = CpuLimits {
                quota: 200_000,
                period: 100_000,
            }
        }
        "pids.limit" => limits.pids = PidsLimits { limit: 32 },
        other => panic!("unknown field {other}"),
    }
    limits
}

#[test]
fn every_field_is_enforced_or_refused_by_name_on_every_tier() {
    // (field, microVM, native+cgroup, native without cgroup / macOS)
    let table = [
        ("wallTimeMs", true, true, true),
        ("outputBytes", false, false, false),
        ("memory.limit", true, true, false),
        ("cpu.quota", true, true, false),
        ("pids.limit", false, true, false),
    ];
    for (field, on_microvm, on_cgroup, on_plain_native) in table {
        for (tier, enforced, accepted) in [
            ("microVM", microvm(), on_microvm),
            ("native+cgroup", native_with_cgroup(), on_cgroup),
            ("native", native_without_cgroup(), on_plain_native),
        ] {
            let result = enforced.check(&only(field));
            if accepted {
                result.unwrap_or_else(|error| {
                    panic!("{tier} must enforce {field}: {}", error.detail)
                });
            } else {
                let error = result.expect_err(&format!("{tier} must refuse {field}"));
                assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{tier} {field}");
                assert!(
                    error.detail.contains(field),
                    "{tier}'s refusal of {field} must name it: {}",
                    error.detail
                );
            }
        }
    }
}

#[test]
fn a_ceiling_the_hypervisor_cannot_size_exactly_is_refused_not_rounded() {
    // 1.5 CPUs: a cgroup enforces it exactly, a hypervisor would have to
    // round to 1 (a ceiling nobody granted) or 2 (wider than the grant).
    let fractional = ResourceLimits {
        wall_time_ms: 1_000,
        cpu: CpuLimits {
            quota: 150_000,
            period: 100_000,
        },
        ..ResourceLimits::default()
    };
    let error = microvm().check(&fractional).expect_err("not whole vCPUs");
    assert!(error.detail.contains("cpu.quota"), "{}", error.detail);
    native_with_cgroup()
        .check(&fractional)
        .expect("cpu.max expresses 1.5 CPUs exactly");

    // Divisible by the broken unit factors (1024 + 1024, 1024 / 1024) but
    // not by one MiB, so this proves the exact unit is checked.
    let not_mib = ResourceLimits {
        wall_time_ms: 1_000,
        memory: MemoryLimits { limit: MIB + 2048 },
        ..ResourceLimits::default()
    };
    let error = microvm().check(&not_mib).expect_err("not a whole MiB");
    assert!(error.detail.contains("memory.limit"), "{}", error.detail);
    native_with_cgroup()
        .check(&not_mib)
        .expect("memory.max takes bytes");

    // 256 vCPUs does not fit krun_set_vm_config's u8.
    let too_many = ResourceLimits {
        wall_time_ms: 1_000,
        cpu: CpuLimits {
            quota: 256 * 100_000,
            period: 0,
        },
        ..ResourceLimits::default()
    };
    microvm().check(&too_many).expect_err("256 vCPUs");
}

#[test]
fn a_cpu_ceiling_outside_the_kernels_range_is_refused_before_the_kernel_sees_it() {
    for cpu in [
        CpuLimits {
            quota: 500,
            period: 100_000,
        },
        CpuLimits {
            quota: 100_000,
            period: 999,
        },
        CpuLimits {
            quota: 100_000,
            period: 1_000_001,
        },
    ] {
        let limits = ResourceLimits {
            wall_time_ms: 1_000,
            cpu,
            ..ResourceLimits::default()
        };
        let error = native_with_cgroup()
            .check(&limits)
            .expect_err("outside cgroup v2's cpu.max range");
        assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{cpu:?}");
    }
}

#[test]
fn a_cpu_period_without_a_quota_is_refused_rather_than_ignored() {
    let limits = ResourceLimits {
        wall_time_ms: 1_000,
        cpu: CpuLimits {
            quota: 0,
            period: 100_000,
        },
        ..ResourceLimits::default()
    };
    for tier in [microvm(), native_with_cgroup(), native_without_cgroup()] {
        let error = tier.check(&limits).expect_err("a period bounds nothing");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("cpu.period"), "{}", error.detail);
    }
}

#[test]
fn a_grant_narrows_cpu_by_bandwidth_not_by_quota_alone() {
    let requested = ResourceLimits {
        cpu: CpuLimits {
            quota: 200_000,
            period: 100_000,
        },
        ..ResourceLimits::default()
    };
    // The same 2 CPUs over a longer period: not wider.
    let restated = ResourceLimits {
        cpu: CpuLimits {
            quota: 400_000,
            period: 200_000,
        },
        ..ResourceLimits::default()
    };
    restated.narrows(&requested).expect("equal bandwidth");
    // A smaller quota over a much shorter period is MORE bandwidth: 3 CPUs.
    let wider = ResourceLimits {
        cpu: CpuLimits {
            quota: 30_000,
            period: 10_000,
        },
        ..ResourceLimits::default()
    };
    let error = wider.narrows(&requested).expect_err("3 CPUs widens 2");
    assert!(error.detail.contains("cpu.quota"), "{}", error.detail);
    // No ceiling where one was requested widens it.
    ResourceLimits::default()
        .narrows(&requested)
        .expect_err("no cpu ceiling widens a requested one");
    // pids, the new dimension, narrows like the others.
    let pids = |limit| ResourceLimits {
        pids: PidsLimits { limit },
        ..ResourceLimits::default()
    };
    pids(16).narrows(&pids(32)).expect("narrower");
    pids(64).narrows(&pids(32)).expect_err("wider");
}

fn native_request(limits: ResourceLimits) -> ExecutionRequest {
    ExecutionRequest {
        run_id: "native-limits-01".into(),
        replay_key: "native-limits-01".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: "a".repeat(64),
        },
        executable: "usr/bin/true".into(),
        arguments: Vec::new(),
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits,
    }
}

/// The defect comment #679 on da36d2 recorded: the native backend took
/// `memory_mib` and `vcpus` and applied neither. Called directly, as an
/// embedder can, it must now refuse by name, and before it spawns anything:
/// the worker path below does not exist, so reaching a spawn would fail with a
/// different error.
#[test]
fn the_native_backend_called_directly_refuses_memory_and_cpu_it_cannot_apply() {
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: PathBuf::from("/nonexistent/leyline-native-worker"),
        cas_root: PathBuf::from("/nonexistent/cas"),
        ephemeral_root: PathBuf::from("/nonexistent/runs"),
        runtime_files: Vec::new(),
        ready_timeout: Duration::from_secs(1),
    });
    assert_eq!(backend.capabilities().enforced, native_without_cgroup());
    for field in ["memory.limit", "cpu.quota", "pids.limit"] {
        let error = match backend.start(&native_request(only(field))) {
            Ok(_) => panic!("{field} accepted by a tier that applies nothing"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{field}");
        assert!(error.detail.contains(field), "{}", error.detail);
    }
    // The wall clock alone gets past the ceiling check, to the configuration
    // check this fixture fails.
    let error = match backend.start(&native_request(only("wallTimeMs"))) {
        Ok(_) => panic!("an unconfigured backend cannot start"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::BackendFailed, "{}", error.detail);
}

/// A backend that records nothing and starts instantly, declaring the tier a
/// vector row names.
struct VectorBackend {
    class: BackendClass,
    enforced: EnforcedCeilings,
}

impl Backend for VectorBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "vector/1".into(),
            backend_class: self.class,
            available: true,
            enforced: self.enforced,
        }
    }
    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "vector/1".into(),
            channels: Vec::new(),
        })
    }
    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(false)
    }
}

fn vector() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/execution/v1/test-vectors/oci-resources.json");
    serde_json::from_slice(&std::fs::read(&path).expect("read vector")).expect("vector JSON")
}

fn resolver(spec: &Value) -> CatalogResolver {
    let executable = &spec["executable"];
    let digest = format!(
        "{}:{}",
        executable["digest"]["algorithm"].as_str().unwrap(),
        executable["digest"]["value"].as_str().unwrap()
    );
    let catalog = json!({"entries": [{
        "artifactDigest": digest,
        "mediaType": executable["mediaType"],
        "rootfs": {"algorithm": "blake3-256", "value": "a".repeat(64)},
        "executable": "bin/build",
        "workspaceInputs": []
    }]});
    CatalogResolver::from_json(catalog.to_string().as_bytes()).expect("catalog")
}

/// The spec's canonical digest, as `authorize` computes it.
fn spec_digest(spec: &Value) -> String {
    use leyline_public_schema::execution_capnp;
    let mut message = capnp::message::Builder::new_default();
    let root = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    capnp_json::from_json(&spec.to_string(), root).expect("spec JSON decodes");
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize");
    leyline_runtime::authorization::canonical_digest(&bytes).expect("digest")
}

/// The bead's closing test: OCI resources that parse, digest, and are refused
/// on the tier that cannot cap pids, accepted on the one that can.
#[test]
fn the_oci_resources_vector_is_refused_where_pids_cannot_be_capped() {
    let vector = vector();
    let spec = &vector["spec"];
    let digest = spec_digest(spec);
    assert_eq!(
        vector["canonicalSpecDigest"].as_str(),
        Some(digest.as_str()),
        "the vector pins the spec's canonical digest"
    );
    assert_eq!(
        vector["grant"]["runSpecDigest"]["value"].as_str(),
        digest.strip_prefix("blake3-256:"),
        "the grant binds the pinned spec"
    );

    let tiers = vector["tiers"].as_array().expect("tiers");
    assert_eq!(tiers.len(), 3);
    for tier in tiers {
        let name = tier["name"].as_str().unwrap();
        let class: BackendClass =
            serde_json::from_value(tier["backendClass"].clone()).expect("backendClass");
        let enforced: EnforcedCeilings =
            serde_json::from_value(tier["enforced"].clone()).expect("enforced");
        let mut grant = vector["grant"].clone();
        grant["backendClass"] = tier["backendClass"].clone();
        let input = json!({"spec": spec, "grant": grant}).to_string();

        let service = ExecutionService::new(VectorBackend { class, enforced });
        service.provision(class, "provision").expect("provision");
        let result = transport::start_json_with_verifier(
            &service,
            &input,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: class,
                required_confinement_digest: None,
            },
            &resolver(spec),
            &MetadataOnlyEvidenceVerifier,
        );
        match tier["outcome"]["refused"].as_str() {
            Some(field) => {
                let error = result.expect_err(&format!("{name} must refuse"));
                assert_eq!(
                    error.code,
                    ErrorCode::UnsupportedBackend,
                    "{name}: {}",
                    error.detail
                );
                assert!(
                    error.detail.contains(&format!("grant limit {field} ")),
                    "{name} must refuse {field} by name: {}",
                    error.detail
                );
            }
            None => {
                result.unwrap_or_else(|error| panic!("{name} must accept: {}", error.detail));
            }
        }
    }
}

/// `capnp_json` skips keys it does not know, so an old-shape request would
/// decode to "no ceiling" and run with none. The carrier refuses the removed
/// keys by name, with the field that replaced each.
#[test]
fn the_json_carrier_refuses_the_removed_limit_keys_by_name() {
    let vector = vector();
    for (object, field, key, replacement) in [
        ("spec", "requestedLimits", "memoryBytes", "memory.limit"),
        ("spec", "requestedLimits", "cpuMillis", "cpu.quota"),
        ("grant", "limits", "memoryBytes", "memory.limit"),
        ("grant", "limits", "cpuMillis", "cpu.quota"),
    ] {
        let mut input = json!({"spec": vector["spec"], "grant": vector["grant"]});
        input[object][field][key] = json!(1024);
        let service = ExecutionService::new(VectorBackend {
            class: BackendClass::MicroVm,
            enforced: microvm(),
        });
        let error = transport::start_json_with_verifier(
            &service,
            &input.to_string(),
            &AuthorizationPolicy::default(),
            &resolver(&vector["spec"]),
            &MetadataOnlyEvidenceVerifier,
        )
        .expect_err("a removed key must not decode to no ceiling");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(
            error.detail.contains(&format!("{object}.{field}.{key}"))
                && error.detail.contains(replacement),
            "{}",
            error.detail
        );
    }
    // Nested keys too: OCI's memory.swap is not modeled, so it is refused
    // rather than accepted and ignored.
    let mut input = json!({"spec": vector["spec"], "grant": vector["grant"]});
    input["spec"]["requestedLimits"]["memory"]["swap"] = json!(0);
    let service = ExecutionService::new(VectorBackend {
        class: BackendClass::MicroVm,
        enforced: microvm(),
    });
    let error = transport::start_json_with_verifier(
        &service,
        &input.to_string(),
        &AuthorizationPolicy::default(),
        &resolver(&vector["spec"]),
        &MetadataOnlyEvidenceVerifier,
    )
    .expect_err("memory.swap is not modeled");
    assert!(error.detail.contains("memory.swap"), "{}", error.detail);
}

#[test]
fn the_microvm_tier_refuses_an_unsized_guest() {
    use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker: PathBuf::from("/nonexistent/leyline-krun-worker"),
        cas_root: PathBuf::from("/nonexistent/cas"),
        ephemeral_root: PathBuf::from("/nonexistent/runs"),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(1),
        tsi_hijack_inet: false,
    });
    assert_eq!(backend.capabilities().enforced, microvm());
    for limits in [only("wallTimeMs"), only("memory.limit"), only("cpu.quota")] {
        let error = match backend.start(&native_request(limits)) {
            Ok(_) => panic!("an unsized microVM must not start"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::InvalidSpec, "{}", error.detail);
        assert!(error.detail.contains("sizes the guest"), "{}", error.detail);
    }
    let error = match backend.start(&native_request(only("pids.limit"))) {
        Ok(_) => panic!("pids on the microVM"),
        Err(error) => error,
    };
    assert!(error.detail.contains("pids.limit"), "{}", error.detail);
}
