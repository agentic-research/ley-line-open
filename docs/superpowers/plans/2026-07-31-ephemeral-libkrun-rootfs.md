# Ephemeral libkrun Rootfs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every libkrun run a private writable userspace rootfs derived from an authenticated immutable CAS root and remove it when the run ends.

**Architecture:** `KrunWorkerBackend` owns a `TempDir` for each child process and passes its exact path to the worker. The worker copies the already-verified rootfs into that empty directory, verifies the copy against the same digest, and grants the confined VMM read/write access only to the copy. Docker/Buildah remain optional OCI producers and are not part of runtime execution.

**Tech Stack:** Rust 2024, `tempfile`, BLAKE3 rootfs manifests, `nono`, libkrun/virtio-fs.

## Global Constraints

- Do not mount or create `/Volumes/krunvm`.
- Do not invoke Docker, Buildah, Podman, `krunvm`, Taskfile, or shell helpers from product code.
- The immutable CAS root remains read-only and outside the post-confinement capability set.
- The parent process, not the confined worker, owns deletion of the per-run directory.
- Portable copy semantics are the baseline; reflink acceleration is a later transparent optimization.

---

### Task 1: Authenticated ephemeral rootfs materializer

**Files:**
- Create: `rs/ll-open/runtime/src/backends/libkrun/volume.rs`
- Modify: `rs/ll-open/runtime/src/backends/libkrun/mod.rs`
- Modify: `rs/ll-open/runtime/src/backends/libkrun/plan.rs`
- Test: `rs/ll-open/runtime/tests/ephemeral_rootfs.rs`

**Interfaces:**
- Consumes: `ResolvedRootfs { digest, canonical_path }` and an empty destination directory.
- Produces: `materialize_ephemeral_rootfs(source: &ResolvedRootfs, destination: &Path) -> Result<ResolvedRootfs, ExecutionError>`.

- [ ] **Step 1: Write failing isolation and verification tests**

```rust
let materialized = materialize_ephemeral_rootfs(&source, run_root.path()).unwrap();
fs::write(materialized.canonical_path.join("usr/bin/probe"), b"guest-write").unwrap();
assert_eq!(fs::read(source.canonical_path.join("usr/bin/probe")).unwrap(), b"probe-v1");
```

- [ ] **Step 2: Verify the tests fail because the module/API does not exist**

Run: `cargo test -p leyline-runtime --test ephemeral_rootfs`
Expected: compilation failure naming the missing `volume` module or `materialize_ephemeral_rootfs`.

- [ ] **Step 3: Implement a symlink-safe regular-file/directory copy and reverify the destination**

```rust
pub fn materialize_ephemeral_rootfs(
    source: &ResolvedRootfs,
    destination: &Path,
) -> Result<ResolvedRootfs, ExecutionError>;
```

- [ ] **Step 4: Verify materialization tests pass**

Run: `cargo test -p leyline-runtime --test ephemeral_rootfs`
Expected: all tests pass.

- [ ] **Step 5: Commit the materializer**

```bash
git add docs/superpowers/plans/2026-07-31-ephemeral-libkrun-rootfs.md rs/ll-open/runtime/src/backends/libkrun/{mod.rs,plan.rs,volume.rs} rs/ll-open/runtime/tests/ephemeral_rootfs.rs
git commit -m "[ley-line-open-7aa13b] feat(runtime): materialize ephemeral rootfs"
```

### Task 2: Parent-owned lifecycle and worker protocol

**Files:**
- Modify: `rs/ll-open/runtime/Cargo.toml`
- Modify: `rs/ll-open/runtime/src/backends/libkrun/backend.rs`
- Modify: `rs/ll-open/runtime/src/backends/libkrun/worker.rs`
- Test: `rs/ll-open/runtime/tests/krun_worker.rs`
- Test: `rs/ll-open/runtime/tests/krun_worker_backend.rs`

**Interfaces:**
- Consumes: `KrunWorkerConfig.ephemeral_root` and `WorkerOptions.run_root`.
- Produces: a parent-owned `WorkerProcess` retaining both `Child` and `TempDir` until reap, cancellation, or drop.

- [ ] **Step 1: Write failing argument and lifecycle tests**

```rust
let run_root = fixture.path().join("runs");
let backend = KrunWorkerBackend::new(KrunWorkerConfig { ephemeral_root: run_root.clone(), ..config });
backend.start(&request).unwrap();
assert_eq!(fs::read_dir(&run_root).unwrap().count(), 1);
drop(backend);
assert_eq!(fs::read_dir(&run_root).unwrap().count(), 0);
```

- [ ] **Step 2: Verify failure names the absent configuration and worker option**

Run: `cargo test -p leyline-runtime --test krun_worker --test krun_worker_backend`
Expected: compilation failures for `ephemeral_root` and `run_root`.

- [ ] **Step 3: Implement the parent-owned TempDir and worker materialization call**

```rust
struct WorkerProcess {
    child: Child,
    _rootfs: tempfile::TempDir,
}
```

- [ ] **Step 4: Verify worker and lifecycle tests pass**

Run: `cargo test -p leyline-runtime --test krun_worker --test krun_worker_backend`
Expected: all tests pass.

- [ ] **Step 5: Commit lifecycle wiring**

```bash
git add rs/ll-open/runtime/Cargo.toml rs/ll-open/runtime/src/backends/libkrun/{backend.rs,worker.rs} rs/ll-open/runtime/tests/{krun_worker.rs,krun_worker_backend.rs}
git commit -m "[ley-line-open-7aa13b] feat(runtime): own per-run rootfs lifecycle"
```

### Task 3: Writable confinement and regression verification

**Files:**
- Modify: `rs/ll-open/runtime/src/backends/libkrun/confinement.rs`
- Test: `rs/ll-open/runtime/tests/libkrun_confinement.rs`

**Interfaces:**
- Consumes: `KrunConfig.rootfs.canonical_path`, now always the ephemeral view in the worker.
- Produces: a nono directory grant with `AccessMode::ReadWrite`; runtime libraries stay read-only and devices stay explicitly read/write.

- [ ] **Step 1: Change the policy assertion to require writable rootfs access**

```rust
assert!(grants.iter().any(|grant| {
    grant.resolved == rootfs.path().canonicalize().unwrap()
        && grant.access == AccessMode::ReadWrite
        && !grant.is_file
}));
```

- [ ] **Step 2: Verify the assertion fails against the read-only policy**

Run: `cargo test -p leyline-runtime --test libkrun_confinement nono_policy_wraps_the_vmm_with_minimal_host_capabilities`
Expected: assertion failure because the rootfs grant is `Read`.

- [ ] **Step 3: Grant read/write access to only the ephemeral root**

```rust
CapabilitySet::new().allow_path(&config.rootfs.canonical_path, AccessMode::ReadWrite)
```

- [ ] **Step 4: Run focused and crate regression tests**

Run: `cargo test -p leyline-runtime`
Expected: all non-ignored tests pass; host-only irreversible tests remain ignored unless explicitly invoked.

- [ ] **Step 5: Commit and update the bead**

```bash
git add rs/ll-open/runtime/src/backends/libkrun/confinement.rs rs/ll-open/runtime/tests/libkrun_confinement.rs
git commit -m "[ley-line-open-7aa13b] feat(runtime): confine writable run roots"
```
