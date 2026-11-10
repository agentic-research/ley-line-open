//! A rootfs imported from an OCI image layout boots and runs its workload in a
//! real libkrun guest (bead `ley-line-open-68a55d`).
//!
//! `runtime/tests/oci_rootfs.rs` proves the import against the resolver and
//! the per-run materialization; no guest booted what it produced. This binary
//! closes that gap on the path a caller takes:
//!
//!   1. a static Linux guest that prints [`MARKER`] and exits 0 is compiled
//!      from the source below, and packed into a two-layer OCI image layout
//!      by the import suite's own generator (`runtime/tests/oci_fixture`);
//!   2. `import_oci_layout` publishes it into the CAS;
//!   3. a `CatalogResolver` binds the guest's executable artifact to the
//!      imported rootfs, and `ExecutionService::start_authorized` runs it over
//!      `KrunWorkerBackend` with the worker under test (the installed one
//!      when `LEYLINE_KRUN_WORKER` names it);
//!   4. the run ends `Succeeded`, the marker is on the guest console, and the
//!      receipt's `inputRoots` names the imported rootfs digest.
//!
//! The negative row tampers with one file under the imported digest (same
//! size, same mode, one byte different) and requires the run refused before
//! the guest boots: the worker, spawned, refuses the tampered file by name
//! while preparing the run and before configuring the VM, leaving no run, no
//! run root, and an empty console. (The libkrun backend verifies the rootfs
//! in the worker; nothing on the host checks it before the worker spawns.)
//!
//! ## The console
//!
//! The guest console is the worker's standard output, which the backend
//! discards. The backend is therefore handed a small host program (compiled
//! below, signed with the hypervisor entitlement the backend checks) that
//! points its standard output at a file and `exec`s the worker under test:
//! same pid, same process group, same descriptors, so the backend supervises
//! the worker itself.
//!
//! ## Synchronization is by event
//!
//! Completion is the backend's completion condition variable
//! (`conformance::Observed::settle`); the console is read only after it.
//! Nothing sleeps. The run's wall clock ([`WALL_TIME_MS`]) bounds a guest that
//! never finishes: a run killed by it with no marker on the console is
//! reported as a suspected `ley-line-open-c4480a` stall, by name, and fails.
//!
//! Gated like every real-guest test: without `LEYLINE_LIBKRUNFW_PATH` each
//! test prints `SKIPPED (NOT a pass)`; `LEYLINE_REQUIRE_HYPERVISOR=1` makes
//! that a failure.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use leyline_runtime::artifacts::oci::{
    DEFAULT_MAX_ROOTFS_BYTES, ImportedRootfs, OciImport, Platform, SymlinkPolicy, import_oci_layout,
};
use leyline_runtime::authorization::{AuthorizationPolicy, canonical_digest};
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::confinement::confinement_manifest;
use leyline_runtime::{
    BackendClass, BackendRunStatus, CatalogBuilder, CatalogResolver, CpuLimits, ExecutionError,
    ExecutionService, MemoryLimits, ResourceLimits, RunRecord, RunState,
};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
// The execution/v1 fixtures this binary shares with the conformance matrix
// (bead ley-line-open-f8ebcf).
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;
// The OCI image-layout generator of the import suite (bead
// ley-line-open-7736ab).
#[path = "../../runtime/tests/oci_fixture/mod.rs"]
mod oci_fixture;

use conformance::{GrantSpec, Observed, Waitable, run_roots};

/// What the guest prints. Nothing else that reaches the console carries it:
/// it is in neither the guest's arguments nor its environment.
const MARKER: &str = "leyline-oci-rootfs-guest-ran-68a55d";

/// Where the executable sits in the image, and so in the guest.
const EXECUTABLE: &str = "bin/marker";

const EXECUTABLE_MEDIA_TYPE: &str = "application/vnd.leyline.executable";

/// The run's wall clock: a bound on a failure only. A guest that boots
/// prints and exits within seconds; one that does not is ended here and
/// reported (see the module documentation).
const WALL_TIME_MS: u64 = 60_000;

const GUEST_SOURCE: &str = r#"use std::io::Write;

fn main() {
    let mut console = std::io::stdout().lock();
    console
        .write_all(concat!(env!("MARKER"), "\n").as_bytes())
        .and_then(|()| console.flush())
        .unwrap_or_else(|_| std::process::exit(70));
}
"#;

impl Waitable for KrunWorkerBackend {
    fn wait(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        self.wait_for_completion(run_id)
    }
}

/// One fixture: an OCI image of the marker guest imported into a CAS, a
/// backend over the worker under test, and the console that worker writes.
struct OciGuest {
    _dir: TempDir,
    imported: ImportedRootfs,
    /// `blake3-256:<hex>` of the guest executable: the RunSpec's artifact.
    artifact: String,
    cas: PathBuf,
    runs: PathBuf,
    console: PathBuf,
    runtime_files: Vec<PathBuf>,
    backend: Observed,
    service: ExecutionService<Observed>,
}

impl OciGuest {
    /// `None` when the host has no hypervisor (a named skip, or a failure
    /// under `LEYLINE_REQUIRE_HYPERVISOR`).
    fn new() -> Option<Self> {
        let libkrunfw = hypervisor::libkrunfw_or_skip()?;
        let dir = TempDir::new().expect("fixture");
        let root = dir.path().canonicalize().expect("canonical fixture");

        let guest = compile_marker_guest(&root);
        let artifact = format!("blake3-256:{}", blake3::hash(&guest).to_hex());

        // A two-layer image: the executable in a gzip layer, an os-release in
        // a plain one, pinned by its manifest digest.
        let platform = Platform::host().expect("an OCI platform for this host");
        let layout_dir = root.join("oci");
        let layout = oci_fixture::Layout::create(&layout_dir);
        let manifest = layout.image(
            &platform.os,
            &platform.architecture,
            &[
                oci_fixture::Layer::gzip(vec![
                    oci_fixture::dir("bin", 0o755),
                    oci_fixture::file(EXECUTABLE, 0o755, &guest),
                ]),
                oci_fixture::Layer::plain(vec![
                    oci_fixture::dir("etc", 0o755),
                    oci_fixture::file("etc/os-release", 0o644, b"ID=leyline-oci-proof\n"),
                ]),
            ],
            None,
        );

        let cas = root.join("cas");
        fs::create_dir(&cas).expect("CAS root");
        let imported = import_oci_layout(
            &OciImport {
                layout: &layout_dir,
                reference: &manifest.digest,
                platform,
                symlinks: SymlinkPolicy::Refuse,
                max_rootfs_bytes: DEFAULT_MAX_ROOTFS_BYTES,
            },
            &cas,
        )
        .expect("import the OCI image layout");
        assert_eq!(imported.image.manifest, manifest.digest);
        assert_eq!(imported.files, 2, "{imported:?}");
        assert_eq!(
            fs::read(cas.join(&imported.rootfs.value).join(EXECUTABLE)).expect("imported guest"),
            guest,
            "the import published the guest's bytes under the rootfs digest"
        );

        let worker = hypervisor::worker_under_test(
            Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
            &root.join("leyline-krun-worker"),
        );
        let console = root.join("console.log");
        let launcher = console_launcher(&root, &worker, &console);
        // The backend spawns the worker with this process's environment, as
        // the execution daemon does; the worker's libkrun loads libkrunfw by
        // name.
        let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
        // SAFETY: every test in this binary holds its serial lock, and
        // nothing else in it reads the environment concurrently.
        unsafe { std::env::set_var(library_var, library_dir) };

        let runs = root.join("runs");
        fs::create_dir(&runs).expect("ephemeral root");
        let runtime_files = vec![libkrunfw];
        let backend = Observed::new(KrunWorkerBackend::new(KrunWorkerConfig {
            worker: launcher,
            cas_root: cas.clone(),
            ephemeral_root: runs.clone(),
            runtime_files: runtime_files.clone(),
            devices: Vec::new(),
            ready_timeout: Duration::from_secs(30),
            tsi_hijack_inet: false,
        }));
        let service = ExecutionService::new(backend.clone());
        let capabilities = service
            .provision(BackendClass::MicroVm, "provision-oci-rootfs")
            .expect("the worker under test can enter the hypervisor");
        assert_eq!(capabilities.backend_id, "libkrun/1");
        Some(Self {
            _dir: dir,
            imported,
            artifact,
            cas,
            runs,
            console,
            runtime_files,
            backend,
            service,
        })
    }

    /// The trusted catalog: the guest's executable artifact runs
    /// [`EXECUTABLE`] in the imported rootfs.
    fn catalog(&self) -> CatalogResolver {
        CatalogBuilder::default()
            .entry(
                self.artifact.clone(),
                EXECUTABLE_MEDIA_TYPE,
                self.imported.rootfs.clone(),
                EXECUTABLE,
                Vec::new(),
            )
            .build()
            .expect("catalog")
    }

    /// Authorize and start the guest through execution/v1. Returns the
    /// start's result, the spec, and the confinement digest the grant named.
    fn start(&self, replay_key: &str) -> (Result<RunRecord, ExecutionError>, Vec<u8>, String) {
        let manifest = confinement_manifest(&self.runtime_files, &[], None)
            .expect("the tier's base confinement document");
        let document = manifest.to_canonical_json().expect("canonical document");
        let digest = manifest.confinement_digest().expect("confinement digest");
        let mut authority = GrantSpec::new(
            BackendClass::MicroVm,
            &format!("{replay_key}-{}", conformance::now_unix_ms()),
            &digest,
            &document,
        );
        authority.executable = (
            EXECUTABLE_MEDIA_TYPE.to_owned(),
            self.artifact
                .strip_prefix("blake3-256:")
                .expect("artifact digest")
                .to_owned(),
        );
        // The hypervisor sizes the guest from memory and CPU, so the tier
        // requires both; the catalog resolves the run's ceilings from the
        // grant.
        authority.limits = ResourceLimits {
            wall_time_ms: WALL_TIME_MS,
            memory: MemoryLimits { limit: 256 << 20 },
            cpu: CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        };
        let (spec, grant) = conformance::spec_and_grant(&authority);
        let started = self.service.start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest.clone()),
            },
            &self.catalog(),
        );
        (started, spec, digest)
    }

    /// Everything the guest console received, or `None` when the worker
    /// never started (the launcher creates the file before it `exec`s).
    fn console(&self) -> Option<String> {
        fs::read(&self.console)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Compile [`GUEST_SOURCE`], with [`MARKER`] substituted at compile time, into
/// a static Linux executable; return its bytes.
fn compile_marker_guest(root: &Path) -> Vec<u8> {
    let source = root.join("marker.rs");
    fs::write(
        &source,
        GUEST_SOURCE.replace("env!(\"MARKER\")", &format!("{MARKER:?}")),
    )
    .expect("guest source");
    let output = root.join("marker");
    hypervisor::compile_guest(&source, &output);
    fs::read(&output).expect("guest bytes")
}

/// A host program that opens `console` (creating it, appending) as its
/// standard output and `exec`s `worker` with its own arguments, signed as the
/// worker is so the backend's entitlement check holds for it.
fn console_launcher(root: &Path, worker: &Path, console: &Path) -> PathBuf {
    let source = root.join("launcher.rs");
    fs::write(
        &source,
        format!(
            r#"use std::os::unix::process::CommandExt;

fn main() {{
    let console = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open({console:?})
        .unwrap_or_else(|error| {{
            eprintln!("console launcher: open the console: {{error}}");
            std::process::exit(111);
        }});
    let error = std::process::Command::new({worker:?})
        .args(std::env::args_os().skip(1))
        .stdout(console)
        .exec();
    eprintln!("console launcher: exec the worker: {{error}}");
    std::process::exit(111);
}}
"#,
            console = console.display().to_string(),
            worker = worker.display().to_string(),
        ),
    )
    .expect("launcher source");
    let launcher = root.join("console-launcher");
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-C", "strip=symbols", "-o"])
        .arg(&launcher)
        .arg(&source)
        .status()
        .expect("compile the console launcher");
    assert!(
        status.success(),
        "console launcher compilation failed: {status}"
    );
    hypervisor::sign_krun_worker(&launcher);
    launcher
}

#[test]
fn an_oci_imported_rootfs_boots_runs_its_workload_and_is_named_by_the_receipt() {
    let _serial = common::serial();
    let Some(guest) = OciGuest::new() else {
        return;
    };
    let (started, spec, digest) = guest.start("oci-rootfs");
    let record = started.expect("start the imported rootfs through execution/v1");
    assert_eq!(record.state, RunState::Running);
    assert_eq!(record.backend_id, "libkrun/1");

    let status = guest.backend.settle(&record.run_id);
    let console = guest.console().unwrap_or_default();
    if let BackendRunStatus::Failed(error) = &status
        && !console.contains(MARKER)
        && error.detail.contains("wall")
    {
        panic!(
            "SUSPECTED ley-line-open-c4480a STALL: the guest printed no marker before \
             its {WALL_TIME_MS} ms wall clock ended the run ({error}).\nconsole:\n{console}"
        );
    }
    assert_eq!(
        status,
        BackendRunStatus::Succeeded,
        "the guest exits 0.\nconsole:\n{console}"
    );
    assert!(
        console.lines().any(|line| line.trim_end() == MARKER),
        "the guest's marker is on its console.\nconsole:\n{console}"
    );
    assert_eq!(run_roots(&guest.runs), 0, "completion removes the run root");
    assert_eq!(
        guest
            .service
            .status(&record.run_id)
            .expect("status")
            .map(|run| run.state),
        Some(RunState::Succeeded)
    );

    let receipt = guest.service.collect(&record.run_id).expect("collect");
    assert_eq!(receipt.run_id, record.run_id);
    assert_eq!(receipt.terminal_state, RunState::Succeeded);
    assert_eq!(receipt.backend_id, "libkrun/1");
    assert_eq!(receipt.context.backend_class, BackendClass::MicroVm);
    assert_eq!(receipt.context.confinement_digest, digest);
    assert_eq!(
        receipt.context.run_spec_digest,
        canonical_digest(&spec).expect("spec digest"),
        "the receipt binds the spec naming the guest's executable artifact"
    );
    assert_eq!(
        receipt.context.input_roots,
        vec![format!("blake3-256:{}", guest.imported.rootfs.value)],
        "the receipt names the imported rootfs"
    );
    assert_eq!(
        guest
            .service
            .cleanup(&record.run_id)
            .expect("cleanup")
            .state,
        RunState::Cleaned
    );
    // The CAS entry is untouched by the run: a run reads a per-run copy.
    assert!(
        guest.cas.join(&guest.imported.rootfs.value).is_dir(),
        "the imported rootfs is still published"
    );
}

#[test]
fn a_tampered_file_under_the_imported_digest_is_refused_before_the_guest_boots() {
    let _serial = common::serial();
    let Some(guest) = OciGuest::new() else {
        return;
    };
    // One byte of the executable, flipped in place: same size, same mode, so
    // only its content digest can tell.
    let target = guest
        .cas
        .join(&guest.imported.rootfs.value)
        .join(EXECUTABLE);
    let mode = fs::metadata(&target)
        .expect("imported file")
        .permissions()
        .mode()
        & 0o7777;
    fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o200)).expect("writable");
    let mut bytes = fs::read(&target).expect("imported bytes");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&target, &bytes).expect("tamper");
    fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("restore mode");

    let (started, _, _) = guest.start("oci-rootfs-tampered");
    let error = started.expect_err("a tampered rootfs never runs");
    assert!(
        error.detail.contains(&format!(
            "rootfs content digest differs from manifest: {EXECUTABLE}"
        )),
        "the refusal names the tampered file: {error:?}"
    );
    assert_eq!(
        run_roots(&guest.runs),
        0,
        "a refused run leaves no run root"
    );
    // The worker was spawned (the launcher created the console) and refused
    // while preparing the run, before it configured the VM: nothing booted,
    // so nothing reached the console. The libkrun backend verifies the
    // rootfs in the worker, not in the host before spawning it.
    assert_eq!(
        guest.console(),
        Some(String::new()),
        "the worker ran and the guest never booted"
    );
}
