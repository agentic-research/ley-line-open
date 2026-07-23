# CI Cache and Release Artifact Design

Status: proposed
Bead: `ley-line-open-7c2361`

## Problem

The v0.10.2 release exposed two related but distinct gaps:

1. Local builds can discover `sccache` through `RUSTC_WRAPPER`, while GitHub
   Actions restores `rs/target` with `rust-cache` but does not install
   `sccache`. Local and runner execution therefore share commands without
   sharing the same cache behavior.
2. A fully restored runner cache did not prevent `rust-lld` from receiving
   `SIGBUS` while linking several large test binaries in parallel. An unchanged
   rerun passed, which makes resource-sensitive linker concurrency the leading
   hypothesis rather than a compilation-cache miss.
3. Release matrix jobs build artifacts and upload them directly with
   `contents: write`. This couples untrusted compiler execution to the release
   write token and makes exact artifact handoff less explicit.

The design must preserve Taskfile as the composable local/CI authority. In
particular, go-task's source/generated-file hashes should suppress redundant
bootstrap work, while `task --force` must intentionally bypass those hashes.

## Considered approaches

### 1. Expand `rust-cache` target caching only

Set `cache-workspace-crates: true` and retain the current workflow structure.
This is the smallest YAML change, but it does not align local builds with CI,
does not cache all compiler invocations as precisely as `sccache`, and does not
address final-link concurrency or release permissions.

### 2. Install `sccache` directly in each workflow

Keep `rust-cache` for Cargo registry/git dependencies and add workflow steps
that install/configure `sccache`. This improves runner performance, but makes
workflow YAML a second implementation of the local bootstrap path. It also
means cache behavior can drift from Taskfile behavior.

### 3. Taskfile-owned cache bootstrap plus build/publish separation

This is the selected approach:

- A pinned, checksum-verified Taskfile task installs or verifies `sccache`.
  It uses `sources`/`generates`, so normal invocations are hash-cached and
  `task --force` deliberately refreshes it.
- Higher-level Taskfile tasks compose bootstrap, build, cache-stat, artifact
  staging, and artifact verification tasks. Workflows invoke those tasks
  instead of reproducing Cargo commands.
- `rust-cache` remains responsible for Cargo registry/git dependencies.
  `sccache` becomes the shared local/runner compiler cache. Whether retaining
  `rs/target` and enabling `cache-workspace-crates` is beneficial is decided by
  measurements rather than enabled blindly.
- Linux CI applies a target-specific internal LLD thread cap. This reduces peak
  link pressure without globally serializing Cargo or penalizing macOS.
- Trusted tag build jobs have read-only permissions, upload immutable workflow
  artifacts, and a downstream publisher with `contents: write` downloads,
  verifies, and uploads those exact bytes to the GitHub release.

This is more structural work than the first two approaches, but it provides one
cache contract, a narrower release credential boundary, and explicit artifact
provenance.

## Task hierarchy

The concrete names may change slightly during implementation, but the
responsibilities remain:

- `cache:sccache:install`: install a pinned binary into a repository-local tool
  directory using OS/architecture selection and an upstream checksum.
- `cache:sccache:verify`: report the resolved binary and require the pinned
  version.
- `cache:sccache:stats`: print hit/miss/error statistics after compilation.
- Existing build and CI tasks depend on the cache bootstrap through composed
  tasks; raw workflow Cargo invocations move behind Taskfile targets.
- Release staging creates a deterministic directory containing the CLI binary,
  optional platform staticlib, optional header, and a SHA-256 manifest.
- Release verification checks the manifest before anything receives release
  write permission.

The repository-local tool directory and transient staged artifacts remain
ignored. The pinned Taskfile executable is authoritative for parity runs.
Local builds retain sccache's platform-default cache directory so sibling
repositories and worktrees share compiler outputs; ephemeral runners set
`SCCACHE_DIR` to `.cache/sccache` so Actions can persist an isolated cache.

## Cache boundaries

- Final release binaries are always built from the tagged source in the trusted
  release workflow. PR artifacts are never promoted.
- Build jobs from the same trusted workflow may pass their finished outputs to
  the publish job.
- Compiler cache entries may be reused only within the cache backend's
  content-addressed and platform/target-compatible scope.
- Local caches may span trusted sibling repositories. Runner caches remain
  scoped by GitHub's branch/cache rules plus OS, architecture, target, Rust
  version, and Cargo lockfile keys.
- The final linker still runs for each target. Caching compiler outputs is not
  treated as proof of final-artifact identity.
- Cache credentials, if a remote backend is later configured, remain
  unavailable to forked/untrusted PR contexts.

## Linker stability

Caching and linker stability are evaluated separately. Linux x86_64 keeps the
Rust 1.97.1 default `rust-lld`, but limits LLD's internal worker count through a
target-specific Cargo/rustflags setting. The initial cap should be conservative
for GitHub's 4-vCPU/16-GB hosted runner while retaining parallel compilation.

If repeated local/container-constrained runs still reproduce `SIGBUS`, compare:

1. baseline default LLD parallelism;
2. capped LLD threads;
3. reduced Cargo jobs;
4. GNU linker fallback via `-Clinker-features=-lld`.

The lowest-cost stable option wins. Cache hit rate is recorded but is not used
as evidence that the linker failure is fixed.

## Release flow

1. Create the GitHub release object idempotently.
2. Matrix build jobs check out the tag with read-only permissions.
3. Each job invokes Taskfile bootstrap/build/stage tasks once for its target.
4. Each job uploads one immutable Actions artifact containing its staged files
   and checksum manifest.
5. A single publish job downloads all matrix artifacts, rejects duplicate
   names or checksum failures, then uploads the verified files with
   `contents: write`.

The header is staged by exactly one matrix entry. Asset names remain compatible
with mache's downloader and existing FFI consumers.

## Validation

Before opening the PR:

- Run a Taskfile parity/lint check that rejects raw Cargo build/test commands in
  the affected workflows and verifies the pinned `sccache` contract.
- Run the bootstrap twice locally and show the second invocation is skipped by
  go-task; run it with `--force` and show it refreshes.
- Run `task ci` with the Taskfile-resolved `sccache`, recording stats.
- Exercise release staging and manifest verification locally for the host
  target without publishing.
- Validate workflow syntax and permissions.
- Run five consecutive constrained Linux `task ci` executions, or the closest
  reproducible 4-vCPU/16-GB equivalent, with no linker signal failures.

CI timing comparisons must distinguish cold bootstrap, warm compiler cache,
Cargo dependency cache, and final linking. A green rerun alone is insufficient
evidence.
