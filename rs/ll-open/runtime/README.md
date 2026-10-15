# leyline-runtime

Capability-resolved execution lifecycle and isolation backends (`execution/v1`). Product policy is resolved before this crate is called — it accepts content identities and guest-relative names; backend-only host paths are introduced behind trusted resolver boundaries.

## What's here

- **`authorization`** — `AuthorizedExecution`, `EvidenceVerifier` (+ `CasDsseEvidenceVerifier`, `MetadataOnlyEvidenceVerifier`, `RejectUnverifiedEvidence`), `SignedGrant`/`GrantSignature`, `SchemaIntent`/`SchemaLimits` — the trust boundary between an execution request and permission to run it.
- **`backends`** — isolation backend implementations: `native` (process), `native_backend`, `libkrun` (microVM). `Backend` is the trait each implements; `BackendCapabilities` / `BackendClass` describe what a backend can enforce.
- **`backends::libkrun`** — the microVM tier. `KrunWorkerBackend` supervises the `leyline-krun-worker` binary; the worker runs the guest through `api::LinkedKrunApi`, libkrun linked from [`third_party/libkrun`](../../../third_party/README.md) (upstream v1.19.4 plus LLO's patches, ADR-0044). `PreparedVm::add_virtiofs_backend` attaches a virtio-fs share whose FUSE requests a `DynFileSystem` in the VMM process answers (re-exported as `api::DynFileSystem` and `api::virtio_fs`).
- **`backends::libkrun::share`** — a run's workspace as the guest's working tree (ADR-0044 phase C, bead `ley-line-open-f210c2`). A run whose RunGrant carries a `WorkspaceGrant` reaches the backend with `ExecutionRequest.workspaces` (carried by the resolver; the service refuses one that changes them). `KrunWorkerBackend::with_workspace_shares` takes the embedder's `WorkspaceShares`; for the run's one granted workspace the backend opens an `FsService` before it spawns the worker, passes one end of a socket pair as `--workspace-share-fd`, and `serve`s the other end. The worker checks the descriptor is a socket matching the grant, attaches `ShareFs(RemoteFs)` (`vmm`; libkrun's `DynFileSystem` transcribing each FUSE request onto the wire) under tag `work`, and sets `KRUN_VIRTIOFS_MOUNTS=work:/work` and the working directory `/work`: the guest init (libkrun patch `0005`) mounts the share before the workload runs. No provider, two workspaces, or a provider refusal refuses the run before anything is spawned. The wire is one frame per request and reply: two little-endian `u32` lengths, a JSON header, and READ/WRITE bytes as raw payload. `leyline-storefs`'s `DaemonShares` is the store-backed provider; `rs/ll-open/cli/tests/agent_in_guest.rs` runs an agent in a guest against it (`task runtime:test`, with `LEYLINE_LIBKRUNFW_PATH` and `LEYLINE_GUEST_TOOLS` from `tools/guest_tools.sh`).

## Features

- **`vmm`** (default) — links libkrun and builds `leyline-krun-worker`. A crate that only supervises the worker turns default features off and does not compile libkrun (`leyline-cli-lib`, `leyline-cli`). Building it compiles libkrun's Linux guest init with `$CC_LINUX`, which the repository's `.cargo/config.toml` points at `tools/krun_init_cc.sh` (the host `cc` on Linux, `zig cc` elsewhere). A consumer outside this repository sets `CC_LINUX` itself or turns the feature off.
- At run time the worker loads one library, libkrunfw (the guest kernel), at `krun_create_ctx`, after its sandbox is applied: grant it with `--runtime-file` and put its directory on the loader path.
- **`confinement`** — ADR-0035's single-declaration manifest: one `cloister/confinement/v1` value from which the applied `nono::CapabilitySet`, the `confinementDigest`, and the digest a backend commits to are all projections, so they cannot drift relative to each other. (Before this existed, `build_process_capabilities` compiled a hardcoded policy with no relationship to the grant's named digest — PR #312 finding 2.)
- **`service`** — `ExecutionService` / `ExecutionResolver`, the orchestration layer driving a request through authorization → backend selection → run.
- **`transport`** — wire-facing types for the execution surface.
- **`CatalogBuilder` / `CatalogResolver`** (`catalog`, private module, re-exported) — resolves declared capabilities against what a backend actually offers.
- **`RunRecord` / `RunState` / `RunReceiptData` / `RunEventRecord`** (`model`, private module, re-exported) — the execution lifecycle's data model, including the DSSE-attestable receipt (see `leyline-envelope`).

## Used by

- **`leyline-cli-lib`** — hosts the `llo_execution_*` daemon ops (`capabilities`, `provision`, `status`, `start`, `inspect`, `cancel`, `collect`, `cleanup`).
- **`leyline-cli`**.
- **cloister** (external, `rs/crates/host-runtime`) — depends on this crate directly via git-rev pin, gated behind its own `llo-execution` feature (`dep:leyline-runtime`), because `leyline-runtime` pulls in `nono` unconditionally as its enforcement mechanism. With default features it also compiles the linked VMM; see `vmm` above.

## Correctness stance

This crate crosses a trust boundary (a signed grant authorizing execution of untrusted content) and is directly consumed outside this repo. See `execution-boundary-adversary` review coverage for RunSpec/RunGrant binding, evidence verification, and backend-parity claims.
