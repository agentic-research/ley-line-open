# CI Cache and Release Artifacts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make local and GitHub Rust builds use the same Taskfile-managed sccache path, cap Linux LLD concurrency, and publish exactly the artifacts produced by read-only release build jobs.

**Architecture:** Taskfile composes a checksum-pinned repository-local sccache bootstrap with CI and release build/staging tasks. GitHub Actions caches the sccache directory, delegates build commands to Taskfile, passes checksum-manifested workflow artifacts between jobs, and confines `contents: write` to release creation/publishing.

**Tech Stack:** go-task 3.49.1, Rust 1.97.1, sccache 0.15.0, POSIX shell, GitHub Actions artifact/cache actions.

## Global Constraints

- Taskfile is the command and cache/bootstrap authority; workflows do not duplicate Cargo build commands.
- `sources`/`generates` make a normal second bootstrap invocation up-to-date; `task --force cache:sccache:install` refreshes it.
- Final releases are built from trusted tag source, never promoted from PR artifacts.
- Build jobs have read-only permissions; only the downstream publishing job receives `contents: write`.
- Cache behavior and Linux `rust-lld` stability are measured independently.
- Rust remains pinned to 1.97.1 and go-task remains pinned to 3.49.1.

---

### Task 1: Pinned Taskfile-owned sccache bootstrap

**Files:**
- Create: `tools/install_sccache.sh`
- Create: `tools/sccache-checksums.txt`
- Modify: `.gitignore`
- Modify: `Taskfile.yml`

**Interfaces:**
- Consumes: `SCCACHE_VERSION=0.15.0`, host `uname -s`/`uname -m`, official Mozilla release archives.
- Produces: `.tools/bin/sccache`, `cache:sccache:install`, `cache:sccache:verify`, `cache:sccache:stats`, and Taskfile `RUSTC_WRAPPER`/`SCCACHE_DIR`.

- [ ] **Step 1: Add the bootstrap contract test before the implementation**

Add `lint:cache-contract` to `Taskfile.yml`. It must require the pinned version,
the installer/checksum sources, generated binary, repository-local
`RUSTC_WRAPPER`, and ignored `.tools/`/`.cache/` directories.

- [ ] **Step 2: Run the contract test and verify it fails**

Run: `task lint:cache-contract`

Expected: FAIL because `tools/install_sccache.sh`,
`tools/sccache-checksums.txt`, and the cache tasks do not exist.

- [ ] **Step 3: Implement the installer and Taskfile tasks**

The checksum manifest contains:

```text
430ef7b5f54256d3ed5bfe77e8b0afc51aa209aeebe4f95b69c3a52ce3acc6e9  sccache-v0.15.0-aarch64-apple-darwin.tar.gz
3a6a3712b49da3d263bf2d30d702de4302793016019e800bfb81c0c69401d8f8  sccache-v0.15.0-aarch64-unknown-linux-musl.tar.gz
f8da93e0689122268f720ddb48c8357f3da18be8c88aff23a8e75a7a219367db  sccache-v0.15.0-x86_64-apple-darwin.tar.gz
782d2b5dd7ae0a55ebe368ab258114d0928d019ac2d949ab85d5d02f3926709e  sccache-v0.15.0-x86_64-unknown-linux-musl.tar.gz
```

The installer maps Darwin/Linux and arm64/aarch64/x86_64, downloads with
`curl --fail --location`, verifies SHA-256, extracts to a temporary directory,
and atomically installs the binary.

The Taskfile install task uses:

```yaml
sources:
  - tools/install_sccache.sh
  - tools/sccache-checksums.txt
generates:
  - .tools/bin/sccache
method: checksum
```

- [ ] **Step 4: Prove go-task caching and force behavior**

Run:

```bash
task cache:sccache:install
task cache:sccache:install
task --force cache:sccache:install
task cache:sccache:verify
```

Expected: first invocation installs 0.15.0, second reports up-to-date, forced
invocation reinstalls, and verification prints `sccache 0.15.0`.

- [ ] **Step 5: Commit**

```bash
git add .gitignore Taskfile.yml tools/install_sccache.sh tools/sccache-checksums.txt
git commit -m "[ley-line-open-7c2361] build(cache): bootstrap pinned sccache"
```

### Task 2: Linux linker stability and CI cache composition

**Files:**
- Create: `.cargo/config.toml`
- Modify: `Taskfile.yml`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `.tools/bin/sccache` and `.cache/sccache` from Task 1.
- Produces: Linux x86_64 `rust-lld` thread cap, cached sccache directory, and post-build cache statistics.

- [ ] **Step 1: Extend `lint:cache-contract` to reject workflow drift**

Require CI to call `task cache:sccache:install`, use a SHA-pinned
`actions/cache`, cache `.cache/sccache`, run `task ci`, and print
`task cache:sccache:stats`. Reject raw `cargo build`, `cargo test`, or
`cargo clippy` commands in `ci.yml`.

- [ ] **Step 2: Run the contract test and verify it fails**

Run: `task lint:cache-contract`

Expected: FAIL because `ci.yml` has no sccache bootstrap/cache/stats steps and
`.cargo/config.toml` has no target-specific linker setting.

- [ ] **Step 3: Implement the minimal workflow/config change**

Add a Linux x86_64 target rustflag that passes `--threads=2` to LLD without
changing Cargo compilation jobs. In `ci.yml`, use
`actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9` for
`.cache/sccache`, keyed by OS, architecture, Rust pin, and Cargo lock hash.
Run the Taskfile bootstrap before `task ci` and emit stats with `if: always()`.
Configure `rust-cache` for dependency reuse without treating workspace crate
target caching as the linker fix.

- [ ] **Step 4: Run focused validation**

Run:

```bash
task lint:cache-contract
task lint:toolchain-parity
actionlint .github/workflows/ci.yml
task cache:sccache:zero
task check
task cache:sccache:stats
```

Expected: all gates pass and stats show compiler requests through sccache.

- [ ] **Step 5: Commit**

```bash
git add .cargo/config.toml Taskfile.yml .github/workflows/ci.yml
git commit -m "[ley-line-open-7c2361] ci(cache): align sccache and cap linux lld"
```

### Task 3: Build once and publish exact release artifacts

**Files:**
- Create: `tools/stage_release_artifacts.sh`
- Create: `tools/verify_release_artifacts.sh`
- Modify: `Taskfile.yml`
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `BUILD_TARGET`, `ASSET_NAME`, optional `LIB_ASSET`, and optional `INCLUDE_HEADER`.
- Produces: `.artifacts/<asset>/` with release files plus `SHA256SUMS`; workflow artifacts named after each matrix asset; verified release upload directory.

- [ ] **Step 1: Add release contract/staging tests**

Extend `lint:cache-contract` to require release YAML to use Taskfile build/stage
tasks, SHA-pinned upload/download actions, read-only build permissions, and one
write-scoped publish job. Add a `release:artifacts:fixture-test` Taskfile task
that stages dummy executable/staticlib/header files, verifies them, corrupts
one byte, and requires verification to fail.

- [ ] **Step 2: Run both tests and verify they fail**

Run:

```bash
task lint:cache-contract
task release:artifacts:fixture-test
```

Expected: contract FAILS on the old direct-upload workflow; fixture task FAILS
because staging/verifying scripts do not exist.

- [ ] **Step 3: Implement Taskfile release build/stage/verify tasks**

Move the CLI and staticlib Cargo invocations behind Taskfile tasks accepting
the target variables. Stage only expected asset names, generate SHA-256 with
portable `shasum -a 256`, and reject missing, extra, duplicate, or mismatched
files during verification.

- [ ] **Step 4: Restructure the release workflow**

Build jobs use `contents: read`, bootstrap/cache sccache, build and stage once,
then upload `.artifacts/<asset>` with
`actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a`.
A downstream `publish` job downloads all outputs with
`actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c`,
invokes Taskfile verification, and uses its sole `contents: write` token to run
one `gh release upload`.

- [ ] **Step 5: Run focused validation**

Run:

```bash
task release:artifacts:fixture-test
task lint:cache-contract
actionlint .github/workflows/release.yml
shellcheck tools/install_sccache.sh tools/stage_release_artifacts.sh tools/verify_release_artifacts.sh
```

Expected: all commands pass.

- [ ] **Step 6: Commit**

```bash
git add Taskfile.yml .github/workflows/release.yml tools/stage_release_artifacts.sh tools/verify_release_artifacts.sh
git commit -m "[ley-line-open-7c2361] ci(release): publish verified build artifacts"
```

### Task 4: Full verification and PR

**Files:**
- Modify: `docs/superpowers/plans/2026-07-23-ci-cache-and-release-artifacts.md`

**Interfaces:**
- Consumes: all earlier tasks.
- Produces: a reviewed branch and GitHub PR for `ley-line-open-7c2361`.

- [ ] **Step 1: Run the full local gate**

Run: `task ci`

Expected: PASS with Rust 1.97.1 and sccache 0.15.0.

- [ ] **Step 2: Run repeat linker/build validation**

Run the strongest available Linux 4-vCPU/16-GB-equivalent link stress loop.
If local Docker is unavailable, record that limitation and use consecutive
GitHub runner executions after opening the PR. No SIGBUS or other linker signal
failure is acceptable.

- [ ] **Step 3: Review the complete diff**

Run:

```bash
git diff --check origin/main...HEAD
git status --short
git log --oneline origin/main..HEAD
```

Expected: only bead-scoped files are changed and the worktree is clean.

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin fix/ley-line-open-7c2361
gh pr create --base main --head fix/ley-line-open-7c2361 --title "ci: align caches and publish verified release artifacts" --body-file /tmp/ley-line-open-7c2361-pr.md
```

- [ ] **Step 5: Watch required checks and repair attributable failures**

Run: `gh pr checks --watch <PR-number>`

Expected: all required checks pass. Record timing/cache/link evidence on the
bead and in the PR.
