use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::process::Command;

use leyline_runtime::DigestRef;
use leyline_runtime::backends::libkrun::confinement::{
    VmmHostResources, apply, build_capabilities, build_process_capabilities,
};
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};
use nono::AccessMode;
use tempfile::TempDir;

fn config(rootfs: &TempDir) -> KrunConfig {
    config_for_path(rootfs.path())
}

fn config_for_path(rootfs: &Path) -> KrunConfig {
    KrunConfig {
        run_id: "run-confinement-01".into(),
        rootfs: ResolvedRootfs {
            digest: DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            canonical_path: rootfs.canonicalize().expect("canonical rootfs"),
        },
        executable: CString::new("usr/bin/probe").expect("exec"),
        arguments: Vec::new(),
        environment: Vec::new(),
        workdir: CString::new("/").expect("workdir"),
        vcpus: 1,
        ram_mib: 512,
        wall_time_ms: 1_000,
        tsi_features: 0,
        port_map: Vec::new(),
    }
}

#[test]
#[ignore = "requires an unsandboxed macOS/Linux host for irreversible enforcement"]
fn nono_enforcement_denies_ambient_host_reads() {
    // Run irreversible sandbox application in a child test process so the
    // parent harness remains usable.
    if std::env::var_os("LEYLINE_NONO_CHILD").is_some() {
        let rootfs = std::env::var_os("LEYLINE_NONO_ROOTFS").expect("child rootfs");
        let runtime_file =
            std::env::var_os("LEYLINE_NONO_RUNTIME_FILE").expect("child runtime file");
        let denied_file = std::env::var_os("LEYLINE_NONO_DENIED_FILE").expect("child denied file");
        let resources = VmmHostResources {
            runtime_files: vec![runtime_file.into()],
            devices: Vec::new(),
        };

        apply(&config_for_path(Path::new(&rootfs)), &resources).expect("apply nono");
        fs::read(Path::new(&rootfs).join("allowed")).expect("read granted rootfs");
        fs::write(Path::new(&rootfs).join("guest-write"), b"ephemeral")
            .expect("write granted ephemeral rootfs");
        let error = fs::read(denied_file).expect_err("ambient read must be denied");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        return;
    }

    let rootfs = TempDir::new().expect("rootfs");
    fs::write(rootfs.path().join("allowed"), b"rootfs").expect("allowed fixture");
    let resources = TempDir::new().expect("host resources");
    let runtime_file = resources.path().join("runtime");
    let denied_file = resources.path().join("ambient-secret");
    fs::write(&runtime_file, b"runtime").expect("runtime fixture");
    fs::write(&denied_file, b"secret").expect("denied fixture");

    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "nono_enforcement_denies_ambient_host_reads",
        ])
        .env("LEYLINE_NONO_CHILD", "1")
        .env("LEYLINE_NONO_ROOTFS", rootfs.path())
        .env("LEYLINE_NONO_RUNTIME_FILE", &runtime_file)
        .env("LEYLINE_NONO_DENIED_FILE", &denied_file)
        .status()
        .expect("spawn sandbox child");

    assert!(status.success(), "sandbox child failed: {status}");
}

#[test]
fn nono_policy_wraps_the_vmm_with_minimal_host_capabilities() {
    // Catches a VMM worker retaining ambient host filesystem or network
    // access after its authenticated rootfs has been selected.
    let rootfs = TempDir::new().expect("rootfs");
    let resources = TempDir::new().expect("host resources");
    let libkrun = resources.path().join("libkrun.dylib");
    let firmware = resources.path().join("libkrunfw.dylib");
    let device = resources.path().join("kvm");
    fs::write(&libkrun, b"library").expect("libkrun fixture");
    fs::write(&firmware, b"firmware").expect("firmware fixture");
    fs::write(&device, b"device").expect("device fixture");

    let capabilities = build_capabilities(
        &config(&rootfs),
        &VmmHostResources {
            runtime_files: vec![libkrun.clone(), firmware.clone()],
            devices: vec![device.clone()],
        },
    )
    .expect("nono capability policy");
    let native_capabilities = build_process_capabilities(
        rootfs.path(),
        &[libkrun.clone(), firmware.clone()],
        std::slice::from_ref(&device),
    )
    .expect("native nono capability policy");

    assert!(capabilities.is_network_blocked());
    let grants = capabilities.fs_capabilities();
    assert!(grants.iter().any(|grant| {
        grant.resolved == rootfs.path().canonicalize().expect("rootfs")
            && grant.access == AccessMode::ReadWrite
            && !grant.is_file
    }));
    for resource in [libkrun, firmware] {
        assert!(grants.iter().any(|grant| {
            grant.resolved == resource.canonicalize().expect("runtime resource")
                && grant.access == AccessMode::Read
                && grant.is_file
        }));
    }
    assert!(grants.iter().any(|grant| {
        grant.resolved == device.canonicalize().expect("device")
            && grant.access == AccessMode::ReadWrite
            && grant.is_file
    }));

    let summarize = |caps: &nono::CapabilitySet| {
        caps.fs_capabilities()
            .iter()
            .map(|grant| {
                (
                    grant.resolved.clone(),
                    format!("{:?}", grant.access),
                    grant.is_file,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        summarize(&capabilities),
        summarize(&native_capabilities),
        "native and libkrun confinement policies must grant the same host paths"
    );
    assert!(native_capabilities.is_network_blocked());
}
