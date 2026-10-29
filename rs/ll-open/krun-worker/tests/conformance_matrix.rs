//! The execution/v1 conformance matrix, embedded libkrun column (bead
//! `ley-line-open-f8ebcf`).
//!
//! The rows are `runtime/tests/conformance/mod.rs`; this binary runs every
//! one of them against `KrunWorkerBackend` supervising the real
//! `leyline-krun-worker`, which links the vendored libkrun and boots a real
//! guest. It lives in this crate because only this crate builds the worker.
//! The native nono column runs the same rows from
//! `runtime/tests/conformance_matrix.rs`; the matrix is documented in
//! `runtime/README.md`.
//!
//! The guest is `runtime/tests/conformance/guest.rs`, compiled as a static
//! Linux program. It dials the host over its §6 `unixSocket.allow` grant,
//! which this tier delivers as a vsock port muxed to the host socket.
//!
//! Gated like every real-guest test: without `LEYLINE_LIBKRUNFW_PATH` each
//! row prints `SKIPPED (NOT a pass)`; `LEYLINE_REQUIRE_HYPERVISOR=1` makes
//! that a failure.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::share::{FsService, WorkspaceShares};
use leyline_runtime::confinement::ConfinementManifest;
use leyline_runtime::{
    BackendClass, BackendRunStatus, CpuLimits, ErrorCode, ExecutionError, MemoryLimits,
    ResourceLimits, WorkspaceGrant,
};
use leyline_storefs::{Options, Store, StoreFs, StoreShare, scope_for};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[macro_use]
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;

use conformance::{
    ChannelKind, Column, ColumnSpec, Denial, Fixture, GraphFixture, Holder, Waitable,
};

const GUEST_EXECUTABLE: &str = "bin/guest";

impl Waitable for KrunWorkerBackend {
    fn wait(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        self.wait_for_completion(run_id)
    }
}

/// The conformance guest, compiled once per binary.
fn guest() -> &'static [u8] {
    static GUEST: OnceLock<Vec<u8>> = OnceLock::new();
    GUEST.get_or_init(|| {
        let scratch = TempDir::new().expect("guest build directory");
        let source = scratch.path().join("guest.rs");
        std::fs::write(&source, conformance::GUEST_SOURCE).expect("guest source");
        let output = scratch.path().join("guest");
        hypervisor::compile_guest(&source, &output);
        std::fs::read(&output).expect("guest bytes")
    })
}

/// This tier's undeliverable channel: §3 `connectLocal`, which a guest
/// behind vsock has no TCP path to.
fn undeliverable(manifest: ConfinementManifest) -> Result<ConfinementManifest, ExecutionError> {
    manifest.with_connect_local(7532)
}

fn libkrun() -> Option<Column> {
    libkrun_with(|backend| backend)
}

/// The libkrun column, its backend configured by `configure`. `None` when
/// the host has no hypervisor (a named skip, or a failure under
/// `LEYLINE_REQUIRE_HYPERVISOR`).
fn libkrun_with(configure: impl FnOnce(KrunWorkerBackend) -> KrunWorkerBackend) -> Option<Column> {
    let libkrunfw = hypervisor::libkrunfw_or_skip()?;
    // The system temp directory, not CARGO_TARGET_TMPDIR: the channel is a
    // UNIX socket under the fixture, and a socket path is capped near 104
    // bytes on macOS. A host path is outside the guest's namespace whatever
    // directory holds it, so the decoy rule of the native column does not
    // apply here.
    let fixture = Fixture::new_in(&std::env::temp_dir());
    let rootfs = fixture.publish_guest(GUEST_EXECUTABLE, guest());
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &fixture.root.join("leyline-krun-worker"),
    );
    // The backend spawns the worker with this process's environment, as the
    // execution daemon does; the worker's libkrun loads libkrunfw by name.
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    // SAFETY: every row holds this binary's serial lock, and nothing else in
    // it reads the environment concurrently.
    unsafe { std::env::set_var(library_var, library_dir) };
    let runtime_files = vec![libkrunfw];
    let backend = configure(KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: fixture.cas.clone(),
        ephemeral_root: fixture.runs.clone(),
        runtime_files: runtime_files.clone(),
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    }));
    Some(Column::new(
        fixture,
        ColumnSpec {
            name: "libkrun",
            class: BackendClass::MicroVm,
            backend_id: "libkrun/1",
            rootfs,
            executable: GUEST_EXECUTABLE.into(),
            arguments: vec!["guest".into()],
            runtime_files,
            channel: ChannelKind::UnixVsock,
            denial: Denial::Absent,
            holder: Holder::ChannelPeer,
            // The hypervisor sizes the guest from memory and CPU, so the tier
            // requires both.
            limits: ResourceLimits {
                wall_time_ms: 120_000,
                memory: MemoryLimits { limit: 256 << 20 },
                cpu: CpuLimits {
                    quota: 100_000,
                    period: 100_000,
                },
                ..ResourceLimits::default()
            },
            undeliverable,
            undeliverable_name: "a §3 connectLocal grant, which the microVM tier cannot deliver",
            // The VMM's own confinement is not the workload's: a host that
            // cannot boot a guest is the hypervisor gate's to report.
            sandbox_refusal: |_| false,
        },
        backend,
    ))
}

conformance_rows! {
    column: libkrun(),
    serial: common::serial(),
    graph: graph_io(),
}

/// The Go source the store is cold-parsed from, and the edit the guest
/// writes over it.
const A: &str = "package a\n\nfunc A() int { return 1 }\n";
const A_EDIT: &str = "package a\n\nfunc A() int { return 2 }\n";

/// A real store: a living database cold-parsed from a one-file tree.
struct GraphStore {
    _dir: TempDir,
    live: Arc<LiveDb>,
    live_db: std::path::PathBuf,
    tree_root: String,
}

impl GraphStore {
    fn new() -> Self {
        let dir = TempDir::new().expect("store directory");
        let source = dir.path().join("src");
        std::fs::create_dir(&source).expect("source tree");
        std::fs::write(source.join("a.go"), A).expect("source file");
        let live_db = dir.path().join("graph.live.db");
        let live = Arc::new(LiveDb::open_fresh_for_test(&live_db));
        live.with_write(|conn| {
            leyline_cli_lib::cmd_parse::parse_into_conn(conn, &source, None, None)?;
            leyline_cli_lib::checkout::ensure_tables(conn)
        })
        .expect("cold-parse the store");
        let tree_root = live
            .with_read(|conn| leyline_cli_lib::tree_root::DerivedFrom::read(conn))
            .expect("read the store's tree root")
            .expect("a cold parse stamps the tree root")
            .tree_root
            .to_string();
        Self {
            _dir: dir,
            live,
            live_db,
            tree_root,
        }
    }

    /// What the daemon's `read_source` answers for `path`.
    fn stored(live: &LiveDb, path: &str) -> Option<Vec<u8>> {
        live.with_read(|conn| {
            Ok(conn.query_row(
                "SELECT b.blob_bytes FROM _tree t \
                   JOIN source_blobs b ON b.blob_hash = t.blob_hash WHERE t.path = ?1",
                [path],
                |row| row.get(0),
            )?)
        })
        .ok()
    }
}

/// Serves the run's granted workspace from [`GraphStore`], opening only at
/// the store's own tree root, as `DaemonShares` does.
struct StoreShares {
    live: Arc<LiveDb>,
    graph_root: String,
}

impl WorkspaceShares for StoreShares {
    fn open(
        &self,
        _run_id: &str,
        grant: &WorkspaceGrant,
    ) -> Result<Arc<dyn FsService>, ExecutionError> {
        if grant.graph_root != self.graph_root {
            return Err(ExecutionError {
                code: ErrorCode::IdentityPolicyMismatch,
                retryable: false,
                detail: format!(
                    "workspace granted at {}, store holds {}",
                    grant.graph_root, self.graph_root
                ),
            });
        }
        Ok(Arc::new(StoreShare::new(StoreFs::new(
            Arc::clone(&self.live),
            scope_for(&grant.operations),
            Options::default(),
        ))))
    }
}

fn graph_io() {
    let store = GraphStore::new();
    let grant = conformance::workspace_grant(&store.tree_root);
    let shares = Arc::new(StoreShares {
        live: Arc::clone(&store.live),
        graph_root: grant.graph_root.clone(),
    });
    let Some(column) = libkrun_with(|backend| backend.with_workspace_shares(shares)) else {
        return;
    };
    let live = Arc::clone(&store.live);
    conformance::row_graph_io_goes_through_the_mediated_workspace_share(
        column,
        GraphFixture {
            grant,
            read: ("a.go", A),
            write: A_EDIT,
            live_db: store.live_db.clone(),
            stored: Box::new(move |path| GraphStore::stored(&live, path)),
        },
    );
}
