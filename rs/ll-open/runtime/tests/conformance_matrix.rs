//! The execution/v1 conformance matrix, native nono column (bead
//! `ley-line-open-f8ebcf`).
//!
//! The rows are `tests/conformance/mod.rs`; this binary runs every one of
//! them against `NativeWorkerBackend` supervising the real first-party
//! `leyline-native-worker`, which verifies the rootfs, applies Seatbelt
//! (macOS) or Landlock (Linux) irreversibly, and only then launches the
//! guest. The embedded libkrun column runs the same rows from
//! `krun-worker/tests/conformance_matrix.rs`; the matrix is documented in
//! `runtime/README.md`.
//!
//! The guest is a copy of this test binary re-entered at
//! [`conformance_guest`], the delivery `native_confinement.rs` uses. It
//! dials the host over its §3 `connectLocal` grant, the channel this tier
//! delivers.
//!
//! A host that cannot apply the sandbox (an outer sandbox; a kernel without
//! Landlock network rules) makes each guest row print `SKIPPED (NOT a pass)`;
//! `LEYLINE_REQUIRE_SANDBOX=1` makes that a failure.

use std::path::PathBuf;
use std::time::Duration;

use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{BackendClass, BackendRunStatus, ExecutionError, ResourceLimits};

mod common;
#[macro_use]
mod conformance;
mod native_guest;

use conformance::{ChannelKind, Column, ColumnSpec, Denial, Fixture, Holder, Waitable};

const WORKER: &str = env!("CARGO_BIN_EXE_leyline-native-worker");
const GUEST_EXECUTABLE: &str = "bin/conformance-guest";

impl Waitable for NativeWorkerBackend {
    fn wait(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        self.wait_for_completion(run_id)
    }
}

/// Not a test: the guest's entry point. The worker launches the copy of this
/// binary in the rootfs with `--ignored --exact conformance_guest`; run
/// unconfined it refuses, because its behavior is the matrix's to choose.
#[test]
#[ignore = "guest-side entry point, launched confined by the native worker; not a harness test"]
fn conformance_guest() {
    assert!(
        std::env::var_os("CONFORMANCE_MODE").is_some(),
        "conformance_guest is the native column's guest; it is meaningless unconfined"
    );
    conformance::guest::run();
}

/// The tier's undeliverable channel: §6 `unixSocket.allow` on Linux, which
/// Landlock at this ABI cannot mediate, and §4 `port.bind` on macOS, which
/// Seatbelt cannot filter by port.
fn undeliverable(manifest: ConfinementManifest) -> Result<ConfinementManifest, ExecutionError> {
    if cfg!(target_os = "linux") {
        manifest.with_unix_socket(UnixSocketGrant::connect("/run/conformance/peer.sock"))
    } else {
        manifest.with_port_bind(7532, None)
    }
}

const UNDELIVERABLE: &str = if cfg!(target_os = "linux") {
    "a §6 unixSocket grant, which Landlock cannot mediate at this ABI"
} else {
    "a §4 port.bind grant, which Seatbelt cannot filter by port"
};

fn native() -> Option<Column> {
    let fixture = Fixture::new_in(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")));
    let guest = std::fs::read(std::env::current_exe().expect("test executable"))
        .expect("read test executable");
    let rootfs = fixture.publish_guest(GUEST_EXECUTABLE, &guest);
    let runtime_files = native_guest::guest_runtime_files();
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: PathBuf::from(WORKER),
        cas_root: fixture.cas.clone(),
        ephemeral_root: fixture.runs.clone(),
        runtime_files: runtime_files.clone(),
        // Materialization copies and re-hashes the guest before readiness;
        // the budget is generous, and readiness is still an event.
        ready_timeout: Duration::from_secs(60),
    });
    Some(Column::new(
        fixture,
        ColumnSpec {
            name: "native-nono",
            class: BackendClass::Native,
            backend_id: "native-nono/1",
            rootfs,
            executable: GUEST_EXECUTABLE.into(),
            arguments: vec![
                "--ignored".into(),
                "--exact".into(),
                "conformance_guest".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            runtime_files,
            channel: ChannelKind::TcpLocal,
            denial: Denial::Kernel,
            holder: Holder::GuestParent,
            // The wall clock only: without a delegated cgroup root the native
            // tier refuses a memory, CPU or pids ceiling (ADR-0035 §4).
            limits: ResourceLimits {
                wall_time_ms: 120_000,
                ..ResourceLimits::default()
            },
            undeliverable,
            undeliverable_name: UNDELIVERABLE,
            sandbox_refusal: native_guest::is_apply_failure,
        },
        backend,
    ))
}

conformance_rows! {
    column: native(),
    serial: common::serial(),
    graph: graph_io_is_refused_on_the_native_tier(),
}

/// The native tier has no workspace-share mechanism: it cannot deliver a
/// Graph workspace, and refuses a grant of one before spawning anything
/// (the `a_workspace_grant_nothing_serves_is_refused_before_spawn` row).
/// That refusal is asserted here too, and the positive Graph I/O row is a
/// named skip, never a pass.
fn graph_io_is_refused_on_the_native_tier() {
    let Some(column) = native() else {
        return;
    };
    conformance::row_a_workspace_grant_nothing_serves_is_refused_before_spawn(column);
    eprintln!(
        "SKIPPED (NOT a pass): Graph I/O through a workspace share is a microVM-tier \
         capability (ADR-0044); the native tier refuses a workspace grant before spawn, \
         which this row asserted. Graph I/O itself is verified by the libkrun column \
         (krun-worker/tests/conformance_matrix.rs)."
    );
}
