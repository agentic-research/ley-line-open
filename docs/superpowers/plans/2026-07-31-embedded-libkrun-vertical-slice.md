# Embedded libkrun Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a pinned content-addressed rootfs through libkrun owned by LLO, with nono confining the VMM process and no invocation of the `krunvm` program.

**Architecture:** A new `leyline-runtime` crate owns backend-independent lifecycle state and a `Backend` trait. Its `libkrun` backend compiles a validated execution request into a host-only plan, dynamically loads libkrun's stable C API, and executes that plan in a dedicated first-party worker process because `krun_start_enter()` consumes the context and exits the process. Before entering the VM, the worker applies a nono capability set granting only the pinned rootfs, required dynamic libraries/firmware, and its explicit broker paths. The first slice accepts an already materialized rootfs whose BLAKE3 digest is verified; OCI pulling/unpacking is a later input adapter.

**Tech Stack:** Rust 2024, `nono = 0.71.0` with default features disabled, `libloading = 0.9.0`, libkrun C API 1.x (developed against 1.19.4), Cap'n Proto execution/v1 types, BLAKE3, Unix process/IPC primitives.

## Global Constraints

- Normal execution MUST NOT invoke `krunvm`, Taskfile, or a repository script.
- The untrusted/public request contains content identities and guest paths, never host paths.
- A trusted resolver verifies the rootfs BLAKE3 digest before producing a host-only `ResolvedKrunPlan`.
- libkrun and the guest are one security context; nono confinement is mandatory before `krun_start_enter`.
- The worker MUST deny ambient networking in this slice. Empty `allowedEgress` is supported; non-empty egress fails closed as `unsupported-backend` until the broker exists.
- `status` is read-only and never materializes a rootfs, creates storage, loads libkrun, or applies nono.
- The root filesystem uses `krun_add_virtiofs3` with tag `/dev/root`, DAX window `0`, and `read_only = true`; writable Graph transport is owned by `ley-line-open-f861c7`.
- Dynamic loading keeps builds/tests usable on hosts without libkrun. Missing libraries or symbols produce typed actionable errors.
- Do not add OCI ingestion, arbitrary host mounts, GPU, inbound ports, TSI networking, Interlace issuance, or APAS construction to this slice.

---

### Task 1: Backend-independent execution core

**Files:**
- Create: `rs/ll-open/runtime/Cargo.toml`
- Create: `rs/ll-open/runtime/src/lib.rs`
- Create: `rs/ll-open/runtime/src/error.rs`
- Create: `rs/ll-open/runtime/src/model.rs`
- Create: `rs/ll-open/runtime/src/service.rs`
- Test: `rs/ll-open/runtime/tests/lifecycle.rs`

**Interfaces:**
- Produces: `ExecutionService<B: Backend>`, `Backend`, `BackendCapabilities`, `ExecutionRequest`, `RunRecord`, `RunState`, `ExecutionError`, and `ErrorCode`.
- `Backend::capabilities(&self)` is read-only.
- `Backend::start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError>` is the only Task 1 mutation boundary.

- [ ] **Step 1: Write the failing lifecycle test**

```rust
#[test]
fn status_before_start_is_read_only() {
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    assert_eq!(service.status("missing").unwrap(), None);
    assert_eq!(backend.calls(), Vec::<String>::new());
}

#[test]
fn repeated_start_with_one_replay_key_returns_the_same_run() {
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    let first = service.start(request("replay-1")).unwrap();
    let second = service.start(request("replay-1")).unwrap();
    assert_eq!(first.run_id, second.run_id);
    assert_eq!(backend.calls(), vec!["start"]);
}
```

- [ ] **Step 2: Run the test and verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test lifecycle`

Expected: Cargo reports that package `leyline-runtime` does not exist.

- [ ] **Step 3: Implement the minimal core**

```rust
pub trait Backend: Send + Sync + 'static {
    fn capabilities(&self) -> BackendCapabilities;
    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError>;
}

pub struct ExecutionService<B> {
    backend: B,
    runs: parking_lot::RwLock<HashMap<String, RunRecord>>,
    replay: parking_lot::RwLock<HashMap<String, String>>,
}
```

Validate `run_id`, `replay_key`, digest algorithms/lengths, backend class, limits, and empty egress before invoking the backend. Store immutable accepted/running records keyed by run ID. `status` takes only a read lock.

- [ ] **Step 4: Run lifecycle tests and verify GREEN**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test lifecycle`

Expected: both lifecycle tests pass.

- [ ] **Step 5: Commit**

```bash
git add rs/Cargo.toml rs/Cargo.lock rs/ll-open/runtime
git commit -m "[ley-line-open-f81567] feat(runtime): add execution lifecycle core"
```

### Task 2: Compile a public request into a digest-verified libkrun plan

**Files:**
- Create: `rs/ll-open/runtime/src/backends/mod.rs`
- Create: `rs/ll-open/runtime/src/backends/libkrun/mod.rs`
- Create: `rs/ll-open/runtime/src/backends/libkrun/plan.rs`
- Test: `rs/ll-open/runtime/tests/libkrun_plan.rs`

**Interfaces:**
- Consumes: `ExecutionRequest`, `ExecutionError`, `ErrorCode` from Task 1.
- Produces: `RootfsResolver`, `ResolvedRootfs`, `KrunConfig`, and `compile_plan`.

```rust
pub trait RootfsResolver: Send + Sync {
    fn resolve(&self, digest: &DigestRef) -> Result<ResolvedRootfs, ExecutionError>;
}

pub struct ResolvedRootfs {
    pub digest: DigestRef,
    pub canonical_path: PathBuf,
}

pub struct KrunConfig {
    pub run_id: String,
    pub rootfs: ResolvedRootfs,
    pub executable: CString,
    pub argv: CStringArray,
    pub env: CStringArray,
    pub workdir: CString,
    pub vcpus: u8,
    pub ram_mib: u32,
}
```

- [ ] **Step 1: Write failing plan tests**

Test that a resolver returns only a canonical directory whose deterministic tree digest matches the request; reject digest mismatch, symlink escape, absolute/`..` guest executable, non-empty egress, zero resources, interior NULs, and undeclared environment.

- [ ] **Step 2: Verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test libkrun_plan`

Expected: unresolved `backends::libkrun::plan` imports.

- [ ] **Step 3: Implement the resolver boundary and plan compiler**

The test resolver may supply a precomputed digest. Production `DirectoryRootfsResolver` canonicalizes the configured CAS root, requires the resolved directory to remain beneath that root, and hashes a sorted `(relative-path, mode, content-digest)` manifest. It never trusts a caller-provided path.

- [ ] **Step 4: Verify GREEN**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test libkrun_plan`

- [ ] **Step 5: Commit**

```bash
git add rs/ll-open/runtime rs/Cargo.lock
git commit -m "[ley-line-open-f839c1] feat(runtime): resolve pinned libkrun plans"
```

### Task 3: Dynamic libkrun API with an injectable contract test

**Files:**
- Create: `rs/ll-open/runtime/src/backends/libkrun/api.rs`
- Create: `rs/ll-open/runtime/src/backends/libkrun/runner.rs`
- Test: `rs/ll-open/runtime/tests/libkrun_contract.rs`

**Interfaces:**
- Produces: unsafe sealed `KrunApi`, safe `KrunContext<A: KrunApi>`, `PreparedVm`, and `prepare_vm`.
- Required symbols: `krun_create_ctx`, `krun_free_ctx`, `krun_set_vm_config`, `krun_add_virtiofs3`, `krun_set_workdir`, `krun_set_exec`, `krun_set_port_map`, and `krun_start_enter`.

- [ ] **Step 1: Write a failing call-order contract test**

```rust
#[test]
fn prepares_read_only_root_without_tsi_or_host_mounts() {
    let api = RecordingKrunApi::default();
    let prepared = prepare_vm(api.clone(), config()).unwrap();
    assert_eq!(api.calls(), [
        "create_ctx",
        "set_vm_config:2:2048",
        "add_virtiofs3:/dev/root:0:true",
        "set_port_map:empty",
        "set_workdir:/workspace",
        "set_exec:/usr/bin/true",
    ]);
    drop(prepared);
    assert_eq!(api.calls().last().unwrap(), "free_ctx");
}
```

- [ ] **Step 2: Verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test libkrun_contract`

- [ ] **Step 3: Implement dynamic loading and the safe context wrapper**

Try `LEYLINE_LIBKRUN_PATH` first, then platform library names. Convert every negative return value into `ExecutionError { code: BackendFailed, retryable: false, detail }`. `Drop` calls `krun_free_ctx` only until ownership is consumed by `start_enter`.

Pass an explicit empty null-terminated port map. Do not rely on libkrun's default networking semantics; the worker's nono network denial is the enforcement boundary.

- [ ] **Step 4: Verify GREEN and missing-library behavior**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test libkrun_contract`

Add a test where the loader path is absent and assert `ErrorCode::UnsupportedBackend` plus a message naming libkrun, never Taskfile or krunvm.

- [ ] **Step 5: Commit**

```bash
git add rs/ll-open/runtime rs/Cargo.lock
git commit -m "[ley-line-open-f839c1] feat(runtime): embed the libkrun API"
```

### Task 4: Mandatory nono confinement for the VMM worker

**Files:**
- Create: `rs/ll-open/runtime/src/isolation/mod.rs`
- Create: `rs/ll-open/runtime/src/isolation/nono.rs`
- Test: `rs/ll-open/runtime/tests/nono_policy.rs`
- Test: `rs/ll-open/runtime/tests/nono_denial.rs`

**Interfaces:**
- Produces: `VmmConfinement`, `build_vmm_capabilities`, and `apply_vmm_confinement`.

```rust
pub struct VmmConfinement {
    pub rootfs: PathBuf,
    pub runtime_libraries: Vec<PathBuf>,
    pub firmware_libraries: Vec<PathBuf>,
    pub broker_paths: Vec<PathBuf>,
}

pub fn build_vmm_capabilities(plan: &VmmConfinement)
    -> Result<nono::CapabilitySet, ExecutionError>;
```

- [ ] **Step 1: Write failing policy tests**

Assert the generated capability set grants rootfs read-only, explicitly named runtime/firmware paths read-only, broker sockets with the narrow socket mode, and blocks network. Assert it contains no home directory, repository root, raw arena, or `/Volumes/krunvm` grant.

- [ ] **Step 2: Verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test nono_policy`

- [ ] **Step 3: Implement capability construction and fail-closed support checks**

Call `Sandbox::support_info()` before starting a worker. Unsupported hosts return `UnsupportedBackend`. In the worker, load libkrun and resolve all libraries before calling `Sandbox::apply_auto(&caps)`, because sandbox application is irreversible.

- [ ] **Step 4: Prove a real child denial**

`nono_denial.rs` spawns the test binary's dedicated child mode, applies the capability set, successfully reads a granted fixture, and fails to open an undeclared temporary file. Unsupported OS/kernel support emits an explicit skip reason.

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test nono_denial -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add rs/Cargo.toml rs/Cargo.lock rs/ll-open/runtime
git commit -m "[ley-line-open-f81567] feat(runtime): confine libkrun workers with nono"
```

### Task 5: First-party worker and opt-in host boot proof

**Files:**
- Create: `rs/ll-open/runtime/src/bin/leyline-executor.rs`
- Create: `rs/ll-open/runtime/src/worker.rs`
- Test: `rs/ll-open/runtime/tests/worker_protocol.rs`
- Test: `rs/ll-open/runtime/tests/libkrun_host.rs`
- Modify: `Taskfile.yml`

**Interfaces:**
- Produces: newline-delimited `WorkerRequest`/`WorkerEvent` JSON on inherited pipes and `task runtime:libkrun:host-test`.

- [ ] **Step 1: Write the failing worker-protocol test**

The test launches `CARGO_BIN_EXE_leyline-executor`, sends a plan that requests
`probe` rather than `start`, and asserts a typed capabilities event. It prepends
a temporary directory to `PATH` containing a trap executable named `krunvm`
that writes a sentinel and exits 99. The worker probe must succeed and the
sentinel must remain absent. This proves observable behavior rather than
grepping implementation text.

- [ ] **Step 2: Verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-runtime --test worker_protocol`

- [ ] **Step 3: Implement the worker boundary**

The parent resolves the rootfs and sends a host-only plan over an inherited pipe. The worker loads libkrun, builds the nono capabilities, applies nono, prepares the VM, emits `ready`, then calls `krun_start_enter`. No long-lived daemon thread calls `start_enter` directly.

- [ ] **Step 4: Add the opt-in host proof**

`libkrun_host.rs` runs only when `LEYLINE_TEST_LIBKRUN_ROOTFS` names a rootfs whose digest is supplied in `LEYLINE_TEST_LIBKRUN_ROOTFS_BLAKE3`. It boots `/usr/bin/true`, asserts exit 0, and records libkrun version/path in test output. Missing variables or unsupported HVF/KVM are explicit skips.

Run on the prepared macOS host:

```bash
task runtime:libkrun:host-test \
  ROOTFS=/absolute/pinned/rootfs \
  ROOTFS_BLAKE3=<verified-digest>
```

- [ ] **Step 5: Commit**

```bash
git add Taskfile.yml rs/ll-open/runtime
git commit -m "[ley-line-open-f839c1] feat(runtime): add confined libkrun worker"
```

### Task 6: Minimal daemon transport needed by Cloister

**Files:**
- Modify: `rs/ll-open/cli-lib/Cargo.toml`
- Modify: `rs/ll-open/cli-lib/src/daemon/mod.rs`
- Create: `rs/ll-open/cli-lib/src/daemon/execution.rs`
- Modify: `rs/ll-open/cli-lib/src/daemon/ops/mod.rs`
- Test: `rs/ll-open/cli-lib/tests/execution_transport.rs`

**Interfaces:**
- Consumes: `ExecutionService<LibkrunBackend>`.
- Produces daemon operations matching execution/v1: capabilities, status, provision, start, inspect, cancel, collect, cleanup.

- [ ] **Step 1: Write a failing transport-parity test**

Send the canonical execution/v1 fixture through the daemon handler with a recording backend. Assert the response code/state/receipt equals a direct Rust service call and that status leaves the resolver/backend call counts unchanged.

- [ ] **Step 2: Verify RED**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-cli-lib --test execution_transport`

- [ ] **Step 3: Implement handlers as adapters only**

Handlers decode generated `leyline_public_schema::execution_capnp` messages, call the service, and encode the same generated response types. No lifecycle state or backend selection is reimplemented in CLI-lib.

- [ ] **Step 4: Verify GREEN**

Run: `cd rs && env RUSTC_WRAPPER= cargo test -p leyline-cli-lib --test execution_transport`

- [ ] **Step 5: Commit**

```bash
git add rs/Cargo.lock rs/ll-open/cli-lib rs/ll-open/runtime
git commit -m "[ley-line-open-f8a079] feat(daemon): expose execution lifecycle"
```

### Task 7: Bounded verification and Cloister handoff

**Files:**
- Modify: `docs/ARCHITECTURE.md`
- Modify: `rs/README.md`
- Modify: `rs/ll-open/runtime/README.md`

- [ ] **Step 1: Document the exact boundary**

Record that `leyline-runtime` owns libkrun+nono, the daemon owns transport, Cloister owns policy-to-RunSpec translation, and OCI ingestion/Graph virtiofs remain separate beads.

- [ ] **Step 2: Run focused verification**

```bash
cd rs
env RUSTC_WRAPPER= cargo fmt --all -- --check
env RUSTC_WRAPPER= cargo test -p leyline-runtime -p leyline-public-schema
env RUSTC_WRAPPER= cargo clippy -p leyline-runtime -p leyline-public-schema --all-targets -- -D warnings
cd ..
task lint:architecture-vocabulary
task lint:doc-claims
git diff --check
```

- [ ] **Step 3: Run the real host proof when the pinned fixture is available**

Use `task runtime:libkrun:host-test` with the digest-pinned fixture. This is required before claiming the backend boots; unit contract tests alone prove only safe API composition.

- [ ] **Step 4: Update Rosary**

Comment exact commits and test evidence on `ley-line-open-f81567`, `ley-line-open-f839c1`, and `ley-line-open-f8a079`. Close only beads whose acceptance criteria are fully observed. Keep `f861c7` open until the capability-filtered Graph reaches the guest.

- [ ] **Step 5: Commit documentation**

```bash
git add docs/ARCHITECTURE.md rs/README.md rs/ll-open/runtime/README.md
git commit -m "[ley-line-open-f839c1] docs(runtime): define embedded VM boundary"
```

The subsequent Cloister plan consumes the generated daemon client, switches `LloExecutionClient` from its compatibility provider to LLO, and removes normal `krunvm` invocation. It starts only after Tasks 1–6 pass; it does not copy runtime logic into Cloister.
