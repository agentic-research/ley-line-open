//! The native tier's cgroup v2 ceilings against a real kernel (bead
//! `ley-line-open-da36d2`, ADR-0035 "OCI resources on execution/v1").
//!
//! Linux only: cgroup v2 exists nowhere else, and on macOS the native tier
//! enforces no memory, CPU or pids ceiling at all (`resource_limits.rs` pins
//! that refusal on every platform).
//!
//! Two layers:
//!
//! - `the_host_hierarchy_*` needs no privilege. It runs wherever the tests
//!   do on Linux, including CI's plain `cargo test`.
//! - `a_delegated_root_*` needs a cgroup the test process may create children
//!   in and migrate processes between, which takes root to set up. It runs
//!   when `LLO_TEST_CGROUP_ROOT` names one; `tools/test_runtime_cgroup.sh`
//!   (`task runtime:test:cgroup`, in `task ci`) builds one with sudo and sets
//!   it. Without the variable these tests say so and pass, and the script is
//!   what makes their absence a failure on Linux CI.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leyline_runtime::backends::cgroup::CgroupRoot;
use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::{
    Backend, BackendRunStatus, CeilingMechanism, CpuLimits, DigestRef, ExecutionRequest,
    MemoryLimits, PidsLimits, ResourceLimits,
};
use tempfile::TempDir;

mod common;

#[test]
fn the_host_hierarchy_is_a_root_that_reports_exactly_what_it_delegates() {
    let host = Path::new("/sys/fs/cgroup");
    let Ok(root) = CgroupRoot::open(host) else {
        eprintln!("skipped: /sys/fs/cgroup is not a cgroup v2 mount on this host");
        return;
    };
    let delegated = fs::read_to_string(host.join("cgroup.subtree_control")).expect("controllers");
    let has = |name: &str| delegated.split_whitespace().any(|c| c == name);
    let enforced = root.enforced();
    for (name, mechanism) in [
        ("memory", enforced.memory),
        ("cpu", enforced.cpu),
        ("pids", enforced.pids),
    ] {
        let expected = if has(name) {
            CeilingMechanism::Cgroup
        } else {
            CeilingMechanism::Unenforced
        };
        assert_eq!(mechanism, expected, "{name} in {delegated:?}");
    }
}

#[test]
fn the_host_hierarchy_refuses_a_directory_that_is_not_cgroup2() {
    let dir = TempDir::new().expect("dir");
    fs::write(dir.path().join("cgroup.subtree_control"), "memory cpu pids").expect("fake");
    assert!(CgroupRoot::open(dir.path()).is_err());
}

fn delegated_root() -> Option<CgroupRoot> {
    let Some(path) = std::env::var_os("LLO_TEST_CGROUP_ROOT") else {
        eprintln!(
            "skipped: LLO_TEST_CGROUP_ROOT is unset; `task runtime:test:cgroup` delegates one"
        );
        return None;
    };
    Some(CgroupRoot::open(Path::new(&path)).expect("LLO_TEST_CGROUP_ROOT is a cgroup v2 root"))
}

fn request(run_id: &str, limits: ResourceLimits) -> ExecutionRequest {
    ExecutionRequest {
        run_id: run_id.into(),
        replay_key: run_id.into(),
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

/// A shell stand-in for `leyline-native-worker`: it reads its request, runs
/// `body` (which can see the cgroup it was placed in), then reports ready.
fn backend(fixture: &TempDir, root: CgroupRoot, run_id: &str, body: &str) -> NativeWorkerBackend {
    let worker = fixture.path().join("worker");
    fs::write(
        &worker,
        format!(
            "#!/bin/sh\nIFS= read -r _\ncg=\"/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)\"\n{body}\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"{run_id}\"}}' >&2\n"
        ),
    )
    .expect("worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("mode");
    let cas_root = fixture.path().join("cas");
    let runs = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("cas");
    fs::create_dir(&runs).expect("runs");
    NativeWorkerBackend::new(NativeWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: runs,
        runtime_files: Vec::new(),
        ready_timeout: Duration::from_secs(10),
    })
    .with_cgroup_root(root)
}

#[test]
fn a_delegated_root_places_the_worker_in_a_run_cgroup_carrying_each_ceiling() {
    let _serial = common::serial();
    let Some(root) = delegated_root() else { return };
    let root_path = root.path().to_path_buf();
    let fixture = TempDir::new().expect("fixture");
    let out = fixture.path().join("observed");
    let run_id = "native-cgroup-01";
    let backend = backend(
        &fixture,
        root,
        run_id,
        &format!(
            "{{ echo \"$cg\"; cat \"$cg/memory.max\" \"$cg/cpu.max\" \"$cg/pids.max\"; }} > {}",
            out.display()
        ),
    );
    let enforced = backend.capabilities().enforced;
    assert_eq!(enforced.memory, CeilingMechanism::Cgroup);
    assert_eq!(enforced.cpu, CeilingMechanism::Cgroup);
    assert_eq!(enforced.pids, CeilingMechanism::Cgroup);

    let limits = ResourceLimits {
        wall_time_ms: 10_000,
        memory: MemoryLimits { limit: 256 << 20 },
        // 1.5 CPUs: a ceiling only a cgroup expresses exactly.
        cpu: CpuLimits {
            quota: 150_000,
            period: 100_000,
        },
        pids: PidsLimits { limit: 32 },
        ..ResourceLimits::default()
    };
    backend.start(&request(run_id, limits)).expect("start");
    assert!(matches!(
        backend.wait_for_completion(run_id).expect("complete"),
        BackendRunStatus::Succeeded
    ));

    let observed = fs::read_to_string(&out).expect("the worker reported its cgroup");
    let lines: Vec<&str> = observed.lines().collect();
    let run_cgroup = PathBuf::from(lines[0]);
    assert_eq!(
        run_cgroup,
        root_path.join(format!("leyline-{run_id}")),
        "the worker ran in its run's cgroup, under the delegated root"
    );
    assert_eq!(lines[1], (256u64 << 20).to_string(), "memory.max");
    assert_eq!(lines[2], "150000 100000", "cpu.max");
    assert_eq!(lines[3], "32", "pids.max");
    assert!(
        !run_cgroup.exists(),
        "the run cgroup is removed after the run"
    );
}

#[test]
fn a_delegated_root_enforces_pids_and_its_teardown_kills_what_left_the_group() {
    let _serial = common::serial();
    let Some(root) = delegated_root() else { return };
    let root_path = root.path().to_path_buf();
    let fixture = TempDir::new().expect("fixture");
    let out = fixture.path().join("observed");
    let run_id = "native-cgroup-02";
    // First leave one process behind in its own session: the process-group
    // kill cannot reach it, so only `cgroup.kill` can empty the cgroup for
    // removal. Then try, in a child shell, to start more processes than
    // pids.max allows; the child dies at the first fork the kernel refuses,
    // and its sleeps hold the rest of the budget. Everything after that
    // forks nothing: `read` and `printf` are shell builtins.
    let backend = backend(
        &fixture,
        root,
        run_id,
        &format!(
            "setsid sleep 300 < /dev/null > /dev/null 2>&1 &\n\
             sh -c 'i=0; while [ $i -lt 16 ]; do sleep 30 & i=$((i+1)); done' 2>/dev/null\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"$cg/pids.events\" > {out}",
            out = out.display()
        ),
    );
    let limits = ResourceLimits {
        wall_time_ms: 10_000,
        pids: PidsLimits { limit: 8 },
        ..ResourceLimits::default()
    };
    backend.start(&request(run_id, limits)).expect("start");
    let _ = backend.wait_for_completion(run_id).expect("complete");

    let events = fs::read_to_string(&out).expect("pids.events");
    let refused: u64 = events
        .lines()
        .find_map(|line| line.strip_prefix("max "))
        .and_then(|count| count.trim().parse().ok())
        .expect("pids.events has a max line");
    assert!(
        refused > 0,
        "forks past pids.max must have been refused by the kernel: {events:?}"
    );
    assert!(
        !root_path.join(format!("leyline-{run_id}")).exists(),
        "teardown emptied and removed the cgroup despite the setsid straggler"
    );
}
