# leyline-runtime

Capability-resolved execution lifecycle and isolation backends (`execution/v1`). Product policy is resolved before this crate is called — it accepts content identities and guest-relative names; backend-only host paths are introduced behind trusted resolver boundaries.

## What's here

- **`authorization`** — `AuthorizedExecution`, `EvidenceVerifier` (+ `CasDsseEvidenceVerifier`, `MetadataOnlyEvidenceVerifier`, `RejectUnverifiedEvidence`), `SignedGrant`/`GrantSignature`, `SchemaIntent`/`SchemaLimits` — the trust boundary between an execution request and permission to run it.
- **`backends`** — isolation backend implementations: `native` (process), `native_backend`, `libkrun` (microVM). `Backend` is the trait each implements; `BackendCapabilities` / `BackendClass` describe what a backend can enforce.
- **`confinement`** — ADR-0035's single-declaration manifest: one `cloister/confinement/v1` value from which the applied `nono::CapabilitySet`, the `confinementDigest`, and the digest a backend commits to are all projections, so they cannot drift relative to each other. (Before this existed, `build_process_capabilities` compiled a hardcoded policy with no relationship to the grant's named digest — PR #312 finding 2.)
- **`service`** — `ExecutionService` / `ExecutionResolver`, the orchestration layer driving a request through authorization → backend selection → run.
- **`transport`** — wire-facing types for the execution surface.
- **`CatalogBuilder` / `CatalogResolver`** (`catalog`, private module, re-exported) — resolves declared capabilities against what a backend actually offers.
- **`RunRecord` / `RunState` / `RunReceiptData` / `RunEventRecord`** (`model`, private module, re-exported) — the execution lifecycle's data model, including the DSSE-attestable receipt (see `leyline-envelope`).

## Used by

- **`leyline-cli-lib`** — hosts the `llo_execution_*` daemon ops (`capabilities`, `provision`, `status`, `start`, `inspect`, `cancel`, `collect`, `cleanup`).
- **`leyline-cli`**.
- **cloister** (external, `rs/crates/host-runtime`) — depends on this crate directly via git-rev pin, gated behind its own `llo-execution` feature (`dep:leyline-runtime`), because `leyline-runtime` pulls in `nono` unconditionally as its enforcement mechanism.

## Correctness stance

This crate crosses a trust boundary (a signed grant authorizing execution of untrusted content) and is directly consumed outside this repo. See `execution-boundary-adversary` review coverage for RunSpec/RunGrant binding, evidence verification, and backend-parity claims.
