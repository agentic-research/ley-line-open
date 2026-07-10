# Changelog

All notable changes to ley-line-open are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project adheres to [Semantic Versioning](https://semver.org/).

Each entry references the bead ID(s) tracking the work in
[rsry](https://github.com/agentic-research/rosary) so the full design
context, scoping notes, and review history are recoverable.

## [Unreleased]

## [0.7.1] — 2026-07-10

**Sheaf correctness — math-friend audit fixes + 91× reframing.**

Patch release for correctness bugs surfaced by a theoretical-foundations audit of the sheaf machinery. Three real bugs shipped fixes (orientation-variant H⁰, silent-stale-serve at cascade diameter > 3, δ⁰-threshold unit error), plus rate-normalization root-cause, doc drift cleanup, id-space partition assertions, and guard-quantity corrections. All fixes come with adversarial regression tests that were verified failing pre-fix.

Wire format unchanged — patch bump. Consumers get more accurate invalidation sets and orientation-invariant H⁰ dimension without any client-side changes required.

### Breaking (internal Rust API)

- **`CellComplex::h0_dimension` now returns `Option<usize>` instead of `usize`.** `None` means the active system exceeded `MAX_DENSE_ELEMENTS` and the exact SVD was refused — previously this case returned `0`, which reads as "no globally consistent section exists" (a semantic wrong answer, not a resource condition). Also fixes a sign bug: the active-row filter is now the symmetric `|δ⁰ᵢ| ≤ EPS` (matching `detect_violations`), so `dim H⁰` is invariant under edge-endpoint orientation. Bead `ley-line-open-4e8a8f` (P2); PR #170. External Rust consumers of `leyline-sheaf::complex::CellComplex::h0_dimension` must handle the `Option` — the only known consumer is ley-line (private) which pins by SHA.

### Changed

- **δ⁰-mode cascade runs to per-edge fixed point** (was hardcoded depth-3 cap). Under-invalidation for region graphs of diameter > 3 fixed; mache's Louvain topologies with hundreds of regions now propagate correctly. Instrumented safety-valve budget (default `usize::MAX`, counter fires on truncation). Bead `ley-line-open-4eef8d` (P3); PR #172.
- **δ⁰ threshold moved to norm space** — was comparing `|δ⁰²_current − δ⁰²_baseline|` to `1e-8`, which is not the squared version of `|δ⁰| > ε` (`|a²−b²| = |a−b|·(a+b)`, scale-dependent). Now compares `|√current − √baseline| > 1e-4` — scale-uniform. Bead `ley-line-open-4f3f6e` (P4); PR #171.
- **Sheaf stalks store token-activity rates, not raw counts** — `complex_build_pass.rs::build_complex` now normalizes activity by observation count so stalks are in `[0,1]` per spec. Absolute thresholds now have a defined regime instead of degenerating to zero/nonzero tests under integer-count inputs. Bead `ley-line-open-4e30d5` (P1); PR #173.

### Fixed

- **Id-space partition asserted** — `EDGE_ID_BASE = 1_000_000` was unasserted; silent map-key collision at ≥1M unique tokens. `add_node` / `add_edge` now panic on out-of-range ids. `set_topology` (wire op) was allocating edge ids from `100` against arbitrary wire region ids — worse-than-audited case caught during fixes — unified on `EDGE_ID_BASE`. Similar `AGREEMENT_EDGE_BASE = 1_000` partition guarded in `ops.rs`. Bead `ley-line-open-4fece1` (P6); PR #175.
- **`assert_cochain_complex` silent-skip replaced with warn** — was silently `return`ing above `MAX_DENSE_ELEMENTS = 1e7`, so the δ¹∘δ⁰ = 0 axiom stopped being checked for large complexes. Now logs at warn when skipping. Bead `ley-line-open-504341` (P7a); PR #175.
- **`NEIGHBORHOOD_MAX_DEPTH` guard replaced** — hop cap doesn't bound response size on co-occurrence graphs (radius-1 balls can be O(V) because a k-mention observation creates a k-clique). Replaced with `NEIGHBORHOOD_MAX_CELLS = 1_000` bounding visited-cell count + `truncated` response field. Bead `ley-line-open-504341` (P7b); PR #175.

### Documentation

- **91× ablation reframing** — `docs/research/sheaf-ablation-study.md` clarifies that the 91× over-invalidation ratio validates the **labeling scheme** (label-prefix path in `regions_touching_files`), NOT the sheaf constants under the cascade / δ⁰ threshold path. The measurement path never invokes `on_change`, never evaluates a δ⁰ threshold. Ablation re-run post P1 (rate normalization): 91.02× ≈ 91.01× original — empirically confirms the reframing (path never reads a stalk). Bead `ley-line-open-50c21d`; PR #175.
- **`cache.rs` learned-weight doc drift corrected** — was claiming co-change-learned edge weights weight the cascade frontier; actual behavior is that weights are exported over the wire for external consumers but NOT used in cascade. Doc updated to match code. Also `learn.rs` typo: `O(α^N)` → `O((1−α)^N)`. Bead `ley-line-open-4f9553` (P5); PR #175.

## [0.7.0] — 2026-07-09

**Substrate consolidation + workspace-deps discipline + measurable moat.**

This release closes the "one canonical home for substrate crates" story: `leyline-sign`, `leyline-schema-spec`, and `leyline-cas-ffi` now live in LLO alone; cloister + signet dropped their forks + depend on LLO. The workspace-deps track (`ley-line-open-3b2f55`) landed in three phases with a mache-native drift gate preventing regression. ADR-0028 shipped source_blobs Phase 1 with F-git compat proof independently verified. The sheaf loop got a granularity-dispatcher router (advisory during the ADR-0026 Phase 2 measurement window). Def/ref extraction gained Python, JavaScript, TypeScript symbol production + qualified method tokens + populated `nodes.source_file`.

### Breaking

- `daemon.sheaf.invalidate` payload key rename: `region_ids` → `invalidated`. Consumers that parsed `region_ids` need to update. Mache PR #500 shipped compat by parsing both; other consumers should migrate. Bead `ley-line-open-1104f2` (re-filed as `2191e1`); PR #147.
- `compat_min_schema_version` bumped from `0.4.1` → `0.6.0`. Clients below v0.6.0 don't know the new tables or the unified topic. Semver-honest floor for the wire changes since v0.6.0.

### Added

- **`daemon.sheaf.invalidate` fine-grained region diff** (default when `ComplexBuildPass` has installed a `region_id → token label` map; production path). Payload `scope: "changed-only"` — `region_ids`/`invalidated` contains only regions whose labels match `changed_files` or start with `<file>:sym:`. Coarse-v1 (`scope: "all-known"`) fallback preserved for pre-topology / consumer-pushed cases. Bead `ley-line-open-e40566`; PR #146.

- **ADR-0028 Phase 1 source_blobs dual-store** — `source_blobs(blob_hash BLAKE3, blob_bytes BLOB)` + `_source.content_hash` REFERENCES FK. Additive; existing `_source.source` behavior unchanged. 5 F-gates (F1s round-trip integrity, F4s cross-generation dedup, F5s cross-file dedup, F-rename, **F-git compat proof** — LLO's BLAKE3 of source bytes matches BLAKE3 of `git cat-file blob <sha>`) + 3 adversarial cases (transaction atomicity, large blob, malformed bytes). Bead `ley-line-open-9e4416`; PR #153. F-git independently verified by adversarial agent across 8 edge cases including the wire-format-prefix check that would silently break unified-CAS composition. Bead `ley-line-open-4b19f2`; PR #156.

- **ADR-0029 CAS-backed workspace design doc** — replaces `git worktree add` as the agent-dispatch primitive with a CAS-backed manifest mount. Third leg of the unified-CAS composition (LLO substrate + rosary + cloister). 5 falsifiability gates (F1w startup, F2w storage, F3w isolation, F4w sub-file, F5w commit fidelity). Design-only; implementation phased. Bead `ley-line-open-4b19f2`; PR #154.

- **ADR-0029 F-test measurement harness** — 5 F-tests capturing worktree-flow baselines for future mount-flow comparison. Bead `ley-line-open-5f9829`; PR #163.

- **Workspace-deps discipline in three phases**:
  - Phase 1 (`ley-line-open-3b2f55`, PR #157): `[workspace.dependencies]` block declared with 19 unified entries.
  - Phase 2 (PR #158): 18 crate manifests migrated to `dep = { workspace = true }`.
  - Phase 3 (PR #161): mache-native drift gate. `rs/tools/cargo-toml-projector` walks `rs/**/Cargo.toml`, projects into SQLite (`workspace_deps` + `crate_deps`), and `smell-rules/workspace_deps_drift.json` fires when a crate declares a literal version for a dep already in `[workspace.dependencies]`. Findings diffed against `docs/smell-baseline.json`. Wired into `task ci` + dedicated `find_smells` GitHub workflow (path-filtered).

- **Substrate consolidation from cloister**:
  - `leyline-sign` — cloister's `rs/crates/sign/` fork absorbed as `host` feature + `leyline-sign-helper` bin (ADR-0019). Cloister depends on LLO's canonical crate via git dep. Bead `ley-line-open-7226e3`; PR #160. Cloister-side deletion: cloister PR #119.
  - `leyline-schema-spec` — new crate at `rs/ll-core/schema-spec/` at 0.1.0. cloister's `cloister-spec/` (48 files) moved byte-identically; `verify_vectors_sha256` unit test enforces SHA-256 digest pins. Bead `ley-line-open-729a7e`; PR #159. Cloister-side deletion: cloister PR #120.

- **Substrate consolidation from signet**: signet's stale `rs/crates/sign/` fork retired entirely (whole `rs/` tree deleted); signet is now pure-Go with references pointing at LLO for any future Rust needs. Signet PR #133.

- **`ed25519-dalek` 2 → 3 bump** in leyline-sign. `SigningKey::from_bytes(seed)` replaces `SigningKey::generate(&mut rng)` — decouples `sign` from `rand_core` version drift. All 24 existing signature/verify tests pass unchanged (wire format preserved — RFC 8032 fixed-seed gate). Bead `ley-line-open-474c0a`; PR #155.

- **Def/ref extraction fidelity** — Python, JavaScript, TypeScript now produce non-empty `node_defs` / `node_refs` rows (previously silent-empty despite parsing succeeding). Method tokens emit both qualified (`Type::method`, `Class.method`) and bare (`method`) forms. Rust trait default methods qualified via `trait_item` walker. JS/TS variable bindings to arrow_function / function_expression extract as defs. `nodes.source_file` populated for every AST-derived row. 9 adversarial fidelity tests; all verified failing pre-fix. Bead `ley-line-open-caf423`; PR #165.

- **Sheaf granularity dispatcher router** — pure `route_query(&SheafState, &[u32]) -> GranularityRecommendation` that recommends per-node vs per-file storage based on δ⁰ distribution. Advisory-only during the measurement window; wired via `LEYLINE_PROFILE=1` logging + `ROUTED_PER_NODE`/`ROUTED_PER_FILE` atomics. Consumer wiring waits for ADR-0026 Phase 2 dual-read. Bead `ley-line-open-5b58ff`; PR #162.

- **ADR-0026 Phase 2.0 F2 read-side measurement infrastructure** — `LEYLINE_PROFILE=1` timing wrappers on `op_query` + downstream helpers + criterion bench + baseline JSON. Bead `ley-line-open-335d34`; PR #152.

- **WAL 15b connection pool** — `DaemonContext::live_db` now uses an r2d2 reader pool (default N=`min(10, available_parallelism())`) + dedicated `Mutex<Connection>` writer. 54× throughput improvement, p99=306µs (matches empirical bench target 290-375µs). Caught 2 pre-existing op-misclassification bugs (`op_query`, `op_agreement` were writes disguised as reads — reader pragma's `query_only=ON` fail-loud found them). Bead `ley-line-open-f0239d`; PR #148.

- **WAL 15a file-backed live_db + WAL pragma** — daemon's live db is file-backed at `<ctrl>.live.db` with `journal_mode=WAL`, `synchronous=NORMAL`, `wal_autocheckpoint=1000`. `snapshot_to_arena` composition with WAL verified. Bead `ley-line-open-98fb67`; PR #143.

- **WAL adversarial coverage**: corruption recovery (PR #144), bloat/crash recovery (PR #149), ENOSPC subprocess-isolated fsize quota (PR #150). All three prove daemon fails loud OR recovers cleanly; no silent torn state.

- **ADR-0026 Phase 1 pointer store** — dual-write to `capnp_blobs` (per-file blob) + `_ast_pointer` alongside row-projected `_ast`. F1 round-trip integrity test (~200+ rows byte-identical). Bead `ley-line-open-3e87ad`; PR #145.

- **Sheaf audit gaps 1–4 fully resolved** — watcher enrichment wiring (PR #138), CellComplex persistence (PR #139), coarse-v1 invalidate emit (PR #140), extension seam design docstrings (PR #141), E2E composition test (PR #142). Sheaf loop proven end-to-end + measurably fast.

- **Unified sheaf-invalidate emit** — single `SheafState::emit_invalidate` helper serves both consumer-driven (`op_sheaf_invalidate`) and watcher-driven paths under one canonical topic `daemon.sheaf.invalidate` with one payload contract. Payload drift impossible by construction. Bead `ley-line-open-1104f2` (re-filed as `2191e1`); PR #147.

- **`license = "AGPL-3.0-or-later"`** field added to 12 crate manifests missing the declaration (matches repo LICENSE). Bead `ley-line-open-b04021`; PR #164.

### Changed

- `daemon.sheaf.invalidate` payload from the watcher path is now fine-grained by default (see Added / Breaking above).

### Deprecated

- `region_ids` payload key on `daemon.sheaf.invalidate` — parsers should read `invalidated` going forward. Removed in this release; consumers on <0.7.0 should upgrade.

## [0.6.0] — 2026-07-08

Tagged (`v0.6.0` git tag exists) but never separately documented in CHANGELOG. The 0.7.0 section above supersedes it — 0.7.0 includes all substrate work from 0.6.0 forward.

## [0.5.9] — 2026-07-07

Patch bump. Real monorepo bug: LLO's parse path panicked at `capnp-0.25.x/src/message.rs:565` when a single `AstNode` / `SourceFile` / `Head` / `BindingRecord` canonical form exceeded the default 8 KiB first-segment budget.

### The failure mode

`capnp::message::Builder::set_root_canonical` requires the output builder to be single-segment (asserted at message.rs:565). The default `HeapAllocator` uses `SUGGESTED_FIRST_SEGMENT_WORDS = 1024` (8 KiB). Source messages > 8 KiB force multi-segment allocation and the assertion panics.

Trigger: **generated Go files in monorepos** — `*.pb.go` protobuf stubs, gRPC generated code, wire/mockgen output. LLO's node_id scheme is path-shaped (`pkg/foo.pb.go/function_declaration/block/statement_list/...`) so a 100-deep nested composite literal produces node_id strings >7 KiB, pushing the AstNode canonical form past 8 KiB. `--lang go` filter works correctly; the panic is downstream at per-record serialization.

### Added

- `leyline_schema_capnp::canonical::write_canonical_message<T, W>` — two-pass canonical serialization helper. Measures the source builder's word count and pre-sizes the canonical builder's first segment via `HeapAllocator::first_segment_words(1.5 × source_words + 16)`. `set_root_canonical`'s single-segment assertion now holds for records of any realistic size.
- 3 test pins: 64 KiB regression test (would panic pre-fix), small-message no-regression check, slack-formula pin.

### Changed

- `cmd_parse.rs` Head / SourceFile / AstNode canonical writes now go through the shared helper.
- `lsp/project.rs` BindingRecord canonical write similarly updated.

### Notes

- Two layers stay cleanly separated: CDC chunking (downstream, arena bytes) is unaffected; this fix is upstream at the per-record serialization boundary.
- Follow-up work deferred (own bead cluster): `--include-generated` opt-in policy + max-AST-depth truncation for `_pb.go`/`_gen.go` files. Generated code produces most of the extreme depths + adds little semantic value per node.

## [0.5.8] — 2026-07-07

Version bump. v0.5.7's tag was cut before the keepalive + doctor work landed; this bump republishes the same feature set under a fresh tag so consumers pin to `v0.5.8` and get the working code. Same content as the v0.5.7 changelog entry below (kept for the version-pin trail); no new work in v0.5.8 beyond re-tagging.

## [0.5.7] — 2026-06-26

Two ops-quality fixes surfaced from real-world use post-v0.5.6:

### Bug: mache's Subscribe treated idle connections as dead

Mache's Subscribe goroutine sets a 60s read deadline on the UDS connection to detect a SIGKILLed daemon (`mache/internal/leyline/socket.go:730`). An idle daemon that never emits real events was tripping that deadline and mache logged `"subscribe: read deadline exceeded, treating connection as dead"` — false-positive; the daemon was alive, just quiet.

Fix: **30s keepalive heartbeat** pushed by the daemon's per-connection event-relay task. Emit shape `{"type":"keepalive","ts":<ms>}` — mache filters by `type == "keepalive"` in `runSubscribeLoop` post-v0.5.7. 30s gives 2× headroom against the 60s deadline. Zero cost when real events are flowing (the `tokio::select!` prefers the event branch).

### New: `leyline doctor` command

LLO skips gracefully at runtime when a language server isn't on PATH (v0.5.2+ surfaces the reason via `EnrichmentStats.skipped`), but that's a runtime-only signal — operators + install scripts (mache, cloister) discover the gap only after a query returns empty. `leyline doctor` reports the pre-flight state.

```
$ leyline doctor
Bundled LSP servers (found):
  ✅ rust-analyzer          /Users/j/.cargo/bin/rust-analyzer  (rust)
  ✅ gopls                  /opt/homebrew/bin/gopls            (go)

Bundled LSP servers (missing — tree-sitter-only fallback):
  ❌ pyright-langserver     not on PATH                        (python)
     → npm install -g pyright
```

- `leyline doctor --json` — machine-readable output for install scripts (cloister etc.)
- `leyline doctor --allow-missing` — exit 0 even with gaps (for CI / cloister install scripts that want to WARN)
- Exit code 1 if any bundled server is missing (unless `--allow-missing`)
- Feature-gated behind `lsp` (same feature as the pass itself; default-on)

### Added

- `SUBSCRIBE_KEEPALIVE_INTERVAL = 30s` in `rs/ll-open/cli-lib/src/daemon/socket.rs`. Documented against the concrete mache read-deadline site.
- Per-connection event-relay task now uses `tokio::select!` to emit keepalive events on interval when no real event is pending.
- New `cmd_doctor` module + `Commands::Doctor { json, allow_missing }` subcommand.
- `install_hint_for(server_cmd)` — install commands per bundled server (`rustup component add rust-analyzer`, `go install golang.org/x/tools/gopls@latest`, `npm install -g pyright`, etc.). Test pin defends against a new bundled language being added without a corresponding hint.

### Consumer note

Post-v0.5.7 daemons emit keepalive events on subscribed connections. Consumers (mache, any future subscriber) should filter by `type == "keepalive"` before forwarding to their own subscribers. Mache's follow-up PR bumps the pin + adds the filter; keepalive events without the filter would surface as spurious `{"type":"keepalive"}` map entries in downstream event channels.

## [0.5.6] — 2026-06-26

Patch bump. Closes the gopls cold-start gap mache surfaced after v0.5.5: rust-analyzer worked end-to-end at ~8s with the active-probe path, but **gopls hung the full 50s per-file budget returning 0 hovers and got skipped**. Diagnosed as workspace-init mis-configuration rather than an indexer race: gopls prefers `workspaceFolders` over `rootUri` for Go module detection, and needs `build.expandWorkspaceToModule: true` to analyze the whole module from a subdirectory-rooted workspace.

### Added

- **`workspaceFolders` in initialize handshake** (`rs/ll-open/lsp/src/client.rs`). LSP servers in general prefer this over the deprecated single-folder `rootUri`; gopls treats it as load-bearing for module discovery. Sent alongside `rootUri` so older servers that only consume `rootUri` aren't broken.
- **`workspace.workspaceFolders: true` + `workspace.configuration: true` capabilities** declared in initialize so the server knows we'll handle workspace-change notifications. gopls in particular checks these.
- **`LspClient::start_with_options(cmd, args, root_uri, init_options)`** — new entry point that accepts optional per-server `initializationOptions`. `LspClient::start` is now a wrapper that passes `None`, preserving the existing call shape for callers that don't need per-server tuning.
- **`initialization_options_for_language(lang)`** in `lsp_pass.rs` — per-language init-options table. gopls gets `{build: {expandWorkspaceToModule: true}, ui.completion.usePlaceholders: false, analyses: {}}`. rust-analyzer + other servers return `None` (no tuning needed; workspace-folders alone is enough).
- **3 new test pins** in `lsp_pass::tests`:
  - `gopls_init_options_includes_expand_workspace` — defends against a refactor that drops the load-bearing option
  - `rust_python_no_init_options` — pin the "no tuning needed" expectation for servers that work with just `workspaceFolders`

### Wire effect

gopls's initialize response is unchanged from the client's perspective (same `result` shape), but server-side behavior is now:
- Workspace expansion via `build.expandWorkspaceToModule` → whole module loaded, not just the immediate folder
- `workspaceFolders` array → gopls treats the root as a proper workspace folder for package indexing
- Subsequent `documentSymbol` and `hover` requests resolve against the loaded module, not the empty workspace

### Notes

- v0.5.5's active-probe loop + readiness wait remain unchanged. They're orthogonal — the probe verifies cache-warmness, the workspaceFolders fix ensures there's actually a cache to warm. gopls would never have populated the cache pre-v0.5.6 even with infinite probe retries.
- `LspClient::start` is kept as a thin wrapper for backward compat. All in-tree callers go through `start_with_options` post-v0.5.6; external consumers (if any) keep working unchanged.

## [0.5.5] — 2026-06-26

Patch bump. Fixes two cold-start issues caught after v0.5.4 shipped:

### Bug 1: per-file timeout silently killed the readiness wait

v0.5.4 introduced `await_ready` with rust-analyzer's budget at 30s, but the existing `PASS_FILE_TIMEOUT = 5s` static ceiling wrapped the entire per-file flow. The outer timeout always tripped first; rust-analyzer hover/def/refs never had a chance to answer even after my v0.5.4 fix. This was a latent bug shipped in v0.5.4 — `task ci` passed because no test exercised the rust-analyzer cold-start path end-to-end.

Fix: replace the static constant with `pass_file_timeout_for_language(lang)` — sum of readiness wait + probe budget + per-symbol loop. rust-analyzer's per-file ceiling is now 95s (60 + 5 + 30); gopls 50s; smaller for other languages.

### Bug 2: `quiescent: true` can lie

rust-analyzer's `experimental/serverStatus quiescent: true` notification means "initial analysis cycle finished" — NOT "hover at arbitrary positions returns content." The on-demand query cache for a specific file may still be cold even after quiescence. The symptom on cold-start was the same `25 symbols, 0 hovers/defs/refs` the readiness-wait fix was supposed to solve.

Fix: active-probe verification. After `await_ready` returns true, issue a real hover request at the first DocumentSymbol's selection range; if the response is `Some(content)` the server actually IS ready for semantic queries on this file. If `None`, back off 1s and retry up to 5 times (5s max additional wait). On exhaustion, log + proceed (degrades to v0.5.4 behavior — symbol-loop runs regardless, individual hovers may return empty).

### Added

- `verify_ready_via_probe` (`rs/ll-open/cli-lib/src/daemon/lsp_pass.rs`) — issues a real hover at a known position, retries with 1s back-off up to 5 times. Adds 0-5s on cold-start, costs nothing on warm-pool reuse.
- `pass_file_timeout_for_language(lang)` — replaces the static `PASS_FILE_TIMEOUT` constant. Dynamic sum of readiness + probe + per-symbol budgets.
- `PROBE_MAX_ATTEMPTS = 5`, `PROBE_BACKOFF = 1s`, `PER_SYMBOL_LOOP_BUDGET = 30s` constants. Test pins prevent silent drift.

### Changed

- `ready_timeout_for_language("rust")` bumped 30s → 60s. Empirical from 50-crate cold cargo workspaces on warm-disk Mac. Bump higher only after real-world pressure surfaces — the probe loop is the real safety net.
- Other languages bumped proportionally (gopls 10→15, java 20→30, etc.) to give cold-starts more headroom.

### Removed

- `PASS_FILE_TIMEOUT` static constant. The per-language `pass_file_timeout_for_language` replaces it.

## [0.5.4] — 2026-06-26

Patch bump. Fixes the **rust-analyzer readiness race** that v0.5.3 surfaced via the `skipped: []` reporting — mache observed `lsp: lib.rs — 25 symbols, 0 defs, 0 hovers, 0 refs` and the pass finished in 639ms. That 639ms is the tell: `documentSymbol` is syntactic and returns immediately, but hover/definition/references need the cargo project model loaded. The pre-fix pass fired all three before rust-analyzer finished indexing.

Holistic redesign rather than per-symbol retry: declare the LSP capabilities that opt-in to indexing-progress notifications, parse them as they arrive, and gate the semantic-query loop on a per-language readiness wait.

### Added

- **`window.workDoneProgress: true`** capability declared in `LspClient::start` (`rs/ll-open/lsp/src/client.rs:110-112`). Without this opt-in rust-analyzer (and most servers) don't emit the `$/progress` begin/report/end lifecycle that signals indexing completion.
- **`experimental.serverStatusNotification: true`** capability declared in the same init handshake. rust-analyzer's `experimental/serverStatus` notification signals `quiescent: true` when the server has finished its current analysis sweep — strictly cheaper to consume than parsing `$/progress` titles.
- **`LspClient::await_ready(timeout)`** (`rs/ll-open/lsp/src/client.rs`). Polls the new `server_ready` flag (flipped by either `experimental/serverStatus quiescent: true` or `$/progress` end for an indexing-titled token). Returns `true` on ready signal, `false` on timeout. Callers continue on either — timeout falls back to "issue queries anyway, accept the empty results if the server didn't finish."
- **`is_readiness_token`** helper — recognizes `$/progress` titles whose case-insensitive match contains `indexing`, `loading`, `workspace`, or `ready`. Covers rust-analyzer (`"rust-analyzer/Indexing"`), gopls (`"Setting up workspace"`), pyright (`"Pyright: Indexing"`), and generic fallbacks.
- **`ready_timeout_for_language`** (`rs/ll-open/cli-lib/src/daemon/lsp_pass.rs`) — per-language readiness timeout. rust-analyzer: 30s (cold cargo workspace can take 10-30s); gopls: 10s; pyright/clangd: 8-10s; zls: 5s; fallback: 5s. Returns `Duration::ZERO` to skip the wait entirely for servers without indexing signals (none today; reserved for future bundled servers).
- **`LspEnrichmentPass` readiness gate** — after `documentSymbol` succeeds, the pass now calls `client.await_ready(ready_timeout_for_language(lang))` BEFORE the hover/def/refs loop. Logs a warning when the wait times out so the operator knows the server didn't quiesce in the budget. Bead `ley-line-open-661727`.
- **`handle_notification` covers `$/progress` and `experimental/serverStatus`** in addition to the prior `textDocument/publishDiagnostics` (`rs/ll-open/lsp/src/client.rs`). 6 new unit tests pin: known-title recognition, non-title rejection, `await_ready` returns true on `quiescent: true`, returns true on `$/progress` end-for-indexing, returns false on timeout-without-signals, returns false on `quiescent: false`.

### Behavior change

Cold rust-analyzer enrichment now waits for the indexer to quiesce before issuing hover/def/refs queries. The single-file enrichment-pass latency budget grows from ~600ms (immediate queries against un-indexed server, all empty) to whatever the indexer needs (typically 5-30s on cold cargo workspaces). The trade is correctness — semantic queries actually return real data instead of zero rows. Subsequent enrichments on the same daemon instance reuse the pooled `LspClient` per language and skip the cold-start wait.

### Notes

- The 30s rust-analyzer ceiling is empirical; bump if real-world cargo workspaces routinely exceed it. The pass logs a warning + continues on timeout, so an over-tight ceiling degrades to v0.5.3 behavior rather than blocking.
- No wire-format change. `EnrichmentStats.skipped` (v0.5.3) is still the surface for any per-language enrich skip; v0.5.4 just makes those skip reasons rarer on rust-analyzer because the pass now actually waits for the indexer.

## [0.5.3] — 2026-06-25

Patch bump. Completes the bead `661727` skip-reason surfacing PR (#117 in v0.5.2): the field was populated on the Rust struct + JSON round-trip tested, but **the capnp wire serialization at `op_enrich` dropped it** — Go consumers (mache) read the daemon response via the capnp-Go bindings and never saw the field. Composes with `[[feedback_never_partial_close]]` — discovered while wiring mache's consumer code; fixing upstream rather than working around.

### Added

- **`EnrichmentStats.skipped` to capnp schema** (`rs/ll-core/public-schema/capnp/daemon.capnp` line 51): `skipped @4 :List(Text) $Json.name("skipped");`. Additive ordinal per ADR-0014; backward compatible.
- **`op_enrich` capnp builder populates `skipped`** (`rs/ll-open/cli-lib/src/daemon/ops.rs`). The Rust→capnp marshal loop now copies each pass's `Vec<String>` into the wire response.
- **Regenerated Go bindings** (`clients/go/leyline-schema/daemon/daemon.capnp.go`): `EnrichmentStats.Skipped()`, `HasSkipped()`, `SetSkipped()`, `NewSkipped(n int32)` accessors generated by `capnp compile -ogo` per `clients/go/leyline-schema/regen.sh`.

### Notes

- v0.5.2's `skipped` field was reachable only via the daemon's JSON UDS path (the `serde::Serialize` derive worked end-to-end). The typed-capnp `op_enrich` response dropped it. v0.5.3 fixes the capnp boundary so the field is present in every wire shape.

## [0.5.2] — 2026-06-25

Patch bump. Absorbs three cloister-side primitives into LLO so cloister can de-vendor its `leyline-sign` fork (boundary principle from bead `5e05e6`), surfaces enrichment-pass skip reasons in the daemon's JSON response so consumers can debug what was skipped and why (unblocks mache-303036), and tightens docs + daemon error strings to reflect post-v0.5.0 substrate reality.

### Added

- **`leyline_sign::cert_chain`** (#115, bead `4a9e5a`). Ed25519 cert-chain verifier + claims parsing for ephemeral Signet certs. Supports the Interlace custom-OID arc (`1.3.6.1.4.1.99999.1.{4,5,6}`) for epoch / peer_fp / scope extensions; missing extensions return `None` rather than error. Generic over input cert bytes — works for any Signet-aware CA. Ported from cloister's vendored fork.
- **`leyline_sign::ffi::lsign_alloc` + `lsign_free`** (#115, bead `4ad9da`). wasm32 linear-memory exports for byte-buffer marshalling. Same calling convention as native cdylib (cbindgen); pointers become 32-bit indices into wasm linear memory for workerd / browsers / WASI hosts.
- **`EnrichmentStats.skipped: Vec<String>`** (#117, bead `661727`). Per-language / per-file skip reasons surfaced in the daemon's JSON enrich response. `LspEnrichmentPass` populates four cases that were previously stderr-only: no bundled server for language, server not on PATH, per-language enrich failure, scope-matched-nothing. Field is `#[serde(skip_serializing_if = "Vec::is_empty")]` so the wire shape stays empty when nothing was skipped (backward-compat).

### Changed

- **`leyline_sign::cms::sign_data` omits `signingTime`** (#116, bead-tracked at PR description). RFC 5652 §5.3 lists signingTime as useful-but-unauthenticated, so omission is spec-legal. Previous behavior emitted a hardcoded `250101000000Z` (literal 2025-01-01) — a "fixed time for deterministic output in tests; real usage could use current time" hack that signed a lie. Removing the hack unblocks wasm32 (no per-host time source contract) and matches cloister's vendored fork. Temporal binding moves to `cert.not_before` / `not_after` (signed by the master at mint time) and application-layer attestation rows' server-timestamped `created_at`.
- **`cmd_daemon` off-loopback bind error** (#114, bead `b8c6f2`). Error string now branches on `mcp_no_auth` and reflects ADR-0022 token-gate reality: default mode says "even with the token gate active, off-loopback bind makes the daemon discoverable to every interface"; `--mcp-no-auth` says "with `--mcp-no-auth` the listener is unauthenticated — off-loopback bind is immediately exploitable." Previous string contradicted the code comment 18 lines above ("The MCP wire has no auth" — stale post-ADR-0022).
- **`GETTING-STARTED.md` runtime-deps clarified** (#113, bead `dc6fbf`). macOS default mount is NFS (uses kernel's native NFS client, needs no extra deps); `brew install fuse-t` is OPTIONAL ("only if you want `--backend fuse` on macOS instead of NFS"). Surfaced when verifying `default_backend()` returns NFS on macOS / FUSE on Linux per `cli-lib/src/cmd_serve.rs:17-23`.

### Removed

- **`leyline_sign::cms::encode_utc_time_now`** (#116). Hardcoded-time helper for the now-removed `signingTime` attribute. Re-introduce only with a per-host time-source contract (`js_sys::Date` in V8, `wasi:clocks` in WASI, `SystemTime::now()` native).

### Wire-format break

- CMS signatures produced by `leyline_sign::cms::sign_data` no longer carry the `signingTime` signed attribute. Verifiers that hard-required it will reject these signatures. RFC 5652 §5.3 spec-compliant verifiers treat it as optional and continue to verify. Cloister already emits this shape; signet and mache don't consume LLO's CMS signer directly. Pre-1.0 LLO per `[[feedback_remove_not_deprecate]]` — version is the signal.

### Bead audit (this cycle)

7 LLO beads closed-with-evidence in addition to the work above (`caf9bf`, `d584f1`, `bbb231`, `bc8256`, `5f7100`, `bb0316`, `6263b9`, `9d70b3`, `b93871`, `61d546`, `2f0289`, `4bb8a0`, `b07a79`). Documented via per-bead comments; the LLO open-bead queue is meaningfully smaller post-cycle.

## [0.5.1] — 2026-06-25

Patch bump. Adds protobuf (`.proto`) tree-sitter language support so the LLO daemon parses `.proto` files into the standard `_ast` / `_source` / `nodes` tables. Downstream consumers (mache, anyone querying the projected SQLite) can now register smell rules + run structural queries over proto sources instead of falling back to regex/shell. Plus the documentation + ADR backlog that built up post-v0.5.0.

### Added

- **Protobuf tree-sitter language** (#106). New `proto` feature on `leyline-ts`; `TsLanguage::Proto` variant with `proto` / `protobuf` aliases; canonical extension `.proto`. `tree-sitter-proto = "0.4"` (coder3101/tree-sitter-proto, proto2 + proto3, MIT). `leyline-cli-lib`'s feature list forces `proto` so the daemon registers it out of the box. End-to-end smoke verified against the Sigstore Fulcio `.proto` corpus (20 protos → `_source.language='proto'` + real proto `_ast` node kinds `service` / `rpc` / `message` / etc.).
- **`docs/ARCHITECTURE.md`** (#105). Canonical three-layer overview (infrastructure / projection engine / consumer surface) + Σ snapshot-loop runtime model + cross-runtime consumer pattern + load-bearing ADR table.
- **4 missing crate READMEs** (#105): `cas-ffi`, `chat-embed`, `sheaf`, `text-search`. Filled coverage gap surfaced by the workspace audit.
- **`task readme:version-check`** (#102). CI gate that catches README OCI-tag drift against `rs/ll-open/cli/Cargo.toml`. Wired into `task ci`.
- **ADR-0023 — Agent-first language facts** (#103). Proposes analyzer-as-library ingestion (Go via `go/packages`, Terraform via `hcl/v2`, Rust via `ra_ap_ide`, TypeScript via Compiler API) layered over the existing tree-sitter floor.
- **ADR-0024 — HDC substrate-identity rewrite** (#103). Retrospective for v0.5.0 — documents the four-piece rewrite + Phase 0B-real empirical numbers (score-fusion α=0.20 → +7.3%; kernel-RBF α=0.40 → +7.7% vs vec-alone).
- **ADR-0025 — HDC compositional-vs-distance use modes** (#104). Pre-registers the next research arc: dual-channel encoder (Phase α), archetype codebook (β), sequence-via-permute (γ), compositional query MCP (δ), then Phase ε decision pre-committed to keep / de-feature / remove against the empirical record.
- **Phase 0B-real validation test** (#103). `rs/ll-open/cli-lib/tests/phase_0b_real_ground_truth.rs`. Asserts the substrate value-prop gate: best fusion-sweep recall@10 clears vec-alone by ≥ 0.02. Currently green at +7.7%.

### Changed

- **README HDC blurb + OCI image tags** (#102, #105). Updated to v0.5.0/v0.5.1 + accurate v0.5.0 HDC description (bundle composition + seeded leaves + popcount-Hamming + ADR cross-links).
- **`rs/ll-open/hdc/README.md`** (#105). Full rewrite — old version described pre-v0.5.0 multi-layer codebook architecture; now covers v0.5.0 substrate identity + Phase 0B-real numbers + ADR-0025 forward-link + cost-vs-vec comparison.

### No wire breakage

The proto language addition is purely additive. Existing consumers querying `_ast` / `_source` / `nodes` see new rows where `language='proto'` and node_kinds from the tree-sitter-proto grammar; nothing about existing language ingestion changed. No schema migration needed; HDC encoding is byte-identical (the proto grammar is parse-only — HDC has no proto canonical map yet, so proto nodes flow into HDC as generic AST shapes without language-specific canonicalization. That can land as a separate follow-up if proto-aware HDC search becomes a need).

## [0.5.0] — 2026-06-24

Minor bump: HDC encoder underwent a full substrate-identity rewrite
across four interdependent pieces, and the agent-first semantic
surface (ADR-0016 + ADR-0020) decomposition shipped end-to-end. **All
persisted `_hdc` rows from prior versions are invalid** — encoder
output changed; re-encode on first run. **Wire shape for
`hdc_search` / `hdc_density` changed** — results carry `{score,
matched_stmts, min_distance}` per row plus top-level
`total_query_stmts`, replacing the old `{distance}` shape.

### Substrate-identity rewrite — HDC at function granularity is now graded

Headline: PRs #94 + #98 + #99 + #100 (beads `ley-line-open-7b5086`
and `ley-line-open-98ac42`). Phase 1 retrieval went from "binary
equality oracle, near-similar pairs saturate at D/2" to graded
structural+lexical similarity at function granularity.

Four pieces, all interdependent:

- **Bundle composition** (#98) — replaces XOR-bind composition with
  majority-bundle. XOR-bind was similarity-perfect-transmitting;
  bundle dampens per-level PRG randomness so structural edit distance
  shows up as Hamming distance.
- **Drop `content_role`** (#98) — second fresh-PRG draw per node;
  hurt the same way fingerprint-keyed `base_vector` switching did.
  Retired together with the unbind algebra.
- **fp-quantize `base_vector`** (#99) — drops `child_count +
  sorted_child_discs` from the signature bytes; 35 distinct base
  vectors total. Child-kind info enters via the bundle composition
  over children. `ModuleCodebook` keeps child-kind bytes inline
  because module-level encoding doesn't recurse into bodies.
- **Seeded leaves** (#100) — token-bearing leaves
  (identifiers/literals/primitive types) carry their byte text in
  `EncoderNode::leaf_content`. Encoder produces char-trigram bundle
  HVs at those leaves. `Ref("getName")` measurably closer to
  `Ref("getEmail")` than to `Ref("xyzqqq")`.

Substrate trade-off documented in `phase_1_hdc_retrieval.rs`: the new
encoder blends structural + lexical similarity. Two functions with
shared identifiers can rank closer than two with shared structure but
disjoint identifiers. Real-world blend correctness is the open
Phase 0B question (recall@K vs hand-built ground truth).

#### Retired with the rewrite

- `query::unbind_child_at_position` — no exact-recovery unbind under
  majority bundle without an explicit cleanup-memory codebook.
- `query::explain_cluster_centroid` — depended on unbind algebra.
- Math-friend's Merkle-reversibility property from PR #94 — bundle
  isn't invertible.

#### Math gates re-shaped, not relaxed

`gate_discriminability_near_clones_collapse`: threshold
`median/4 → median × 0.7`. The property is now "near-clones cluster
measurably below random pairs," not "near-clones collapse to identical
HV." Substrate-identity shift, not moved goalpost — the old threshold
was correct for kind-only encoding, not for token-aware encoding.

`validation_gate_full_stack_on_real_go`: identifier-renamed Type-2
clones cluster at d ~885 instead of collapsing to identical HVs. Test
checks the cluster property (d < 2000) rather than exact identity.

### Falsifiability infrastructure — Phase 0 / 0B / 0C / 1 / 1C

Five characterization tests under `rs/ll-open/cli-lib/tests/`, all
`#[ignore]`-gated. Reproduce per each test's docstring.

- **Phase 0** (#91) — synthetic HDC popcount vs vec cosine sim
  throughput. HDC popcount **9.16× faster per pair than naive vec
  cosine** in release at N=10,000 (Apple Silicon).
- **Phase 0B** (#92, re-measured post-rewrite) — Jaccard@K agreement
  on real corpus. **Mean Jaccard@9 went from 0.037 (≈ random under
  old XOR-bind encoder) → 0.234 (partial overlap under the new
  substrate).** The substrate-identity rewrite turned HDC from
  "binary equality oracle that happened to be orthogonal-to-vec
  because both were measuring different unrelated things" to "graded
  similarity that overlaps with vec's lexical signal because seeded
  leaves brought lexical signal into HDC."
- **Phase 0C** (#94, re-measured post-rewrite) — mutual information
  on full rank vectors with closed-form null distribution. **Observed
  MI went from 0.049 nats / 1.2σ above null (degenerate) → 0.072 nats
  / 4.35σ above null (statistically significant signal).** Fraction
  of theoretical max only 3.13% — the agreement is meaningful but
  loose. Neither redundant nor orthogonal: backends order the corpus
  correlatedly but not identically. Whether this means HDC is adding
  value over vec or just being a noisier proxy of it requires ground
  truth (Phase 0B-real, recall@K vs hand-built answers).
- **Phase 1** (#95, updated #100) — load-bearing characterization.
  Originally documented saturation (SUM_A↔SUM_B at d=4115).
  Post-rewrite, assertions inverted: now passes with `SUM_A → top-3
  includes SUM_B at d=6`, `MATCH_A → top-5 includes MATCH_B`.
- **Phase 1C** (#96, obsolete #100) — statement-level set-overlap
  workaround. Bundle composition fixed the underlying problem at the
  encoder level, making the workaround unnecessary.

### Agent-first semantic surface — ADR-0016 + ADR-0020 (L1–L10)

Ten beads shipped end-to-end (#82–#89), closing
`docs/problems/agent-first-semantic-surface.md`.

- **L1 (#82, `c2c4d9`)**: `inspect_symbol(symbol_id) → Bundle` —
  ADR-0016 §2 spine op. Definitions + hover + refs + callers +
  callees + freshness in one round-trip.
- **L2 (#83, `c2e602`)**: `at_position(file, line, col) → {symbol_id, kind}`
  — ADR-0016 §1 editor bridge.
- **L3 (#85, `c77690`)**: `inspect_neighborhood(id, depth,
  edge_kinds, max_bytes)` — ADR-0016 §5. Focal bundle + N-hop
  neighborhood with depth + byte-budget bounds.
- **L4 (#86, `c79953`)**: `search_symbols(pattern, limit, kind?)`
  NDJSON op — ADR-0016 §6 streaming search over `node_defs.token`.
- **L7 (#84, `c3555f`)**: provenance + certainty on inspect_symbol
  bundles — consumers can filter by source-trust.
- **L8 (#87, `c7c79a`)**: `observation` SQL table + SessionObservation
  Pass — ADR-0020 §1 Gate 1. Populates from Claude Code session JSONL.
- **L9 (#88, `c7eae2`)**: ComplexBuildPass — ADR-0020 §2 Gate 2.
  Builds `CellComplex` + drives `CoChangeTracker` over `observation`
  rows. Spy-verified mechanical reach.
- **L10 (#89, `c8090f`)**: `agreement` op via `detect_violations` —
  ADR-0020 §3 Gate 3. Returns `coherence_defect` + per-pair `defects`.

Plus post-merge math-friend hardening (#90, bead `659a39`):
- HIGH: silent dim-mismatch in agreement op — fixed via explicit
  `{ok:false, error:"incompatible_stalk_dims"}` envelope.
- MED: Gate 3 spy counter placement — moved past the algebra so it
  proves math ran, not just function called.
- LOW: Gate 2 vacuous-pass via `nnz(δ⁰) > 0` — tightened with
  `detect_violations().is_empty() == false`.

### Rust defs / refs / imports — extract_rust

**#93 (bead `ley-line-open-117693`)** — closes the gap where
`node_defs` stayed empty for Rust files because `extract_refs` was
Go-only. Mache and other consumers can now treat Rust corpora the same
way they treat Go.

Coverage: `function_item`, `function_signature_item`, `struct_item`,
`enum_item`, `union_item`, `trait_item`, `type_item`, `mod_item`,
`const_item`, `static_item`. Call refs (bare / method / scoped —
scoped emits both qualified and bare). Macro invocations. `use_declaration`
(bare/scoped/aliased/list/nested-list). Wildcards intentionally skipped.

### HDC ops wired + populate pass

- **`hdc_search` / `hdc_density` / `hdc_calibrate` daemon ops** (#79,
  bead `c32596`) — math-friend-reviewed wiring of the HDC retrieval
  surface.
- **`HdcEnrichmentPass`** (#81, bead `641809`) — function-level
  populate pass with math-gate test coverage.

### FastEmbedder for the `vec` backend

**#91** — `fastembed = "5"` (BGESmallENV15 / MiniLM-L6, 384-dim) as a
real `Embedder` impl. Replaces `ZeroEmbedder` for callers opting into
vec retrieval. Three contract tests gated `#[ignore]` (the ONNX model
is a 22MB download).

### Claude Code plugin wrapper

**#78 (bead `cloister-acbf27`)** — Claude Code plugin under
`wrappers/claude-code/` that watches session JSONL transcripts and
emits stale-sync events to the daemon.

### CI optimization

**#74** — Swatinem rust-cache + main-only `test:perf` gate. PR CI
~6.5 min → ~1-2 min on cache hits.

### Version sync fix

All Cargo.toml versions bumped `0.4.5 → 0.5.0`. The version was
drifting in-tree across v0.4.6/v0.4.7/v0.4.8 tags; this release fixes
the drift so `BINARY_VERSION` (read from `CARGO_PKG_VERSION` at build
time) matches the tag.

## [0.4.5] — 2026-05-19

Patch release shipping the mache-bead campaign: typed event payloads,
wire-format handshake, self-maintaining compatibility artifact, and
HCL/Terraform parse support. All four work together — the handshake
op surfaces the same constants the compatibility artifact derives from;
the typed event-payload structs replace mache's hand-rolled
`parseUint64` (which was one of the bugs the handshake exists to
detect at connect time).

### Added — `leyline_version` handshake op (ley-line-open-cb8960)

- New op: `{"op":"leyline_version"}` returns binary_version,
  schema_version, wire_format_major, compat_min, build_date.
- Clients call once at connect to detect version drift up front
  instead of suffering silent `parseUint64`-returns-0 behaviour
  downstream.
- Constants live in `rs/ll-open/cli-lib/src/daemon/version.rs` —
  single source of truth for the substrate's version identity.
- Capnp: new `LeylineVersionResponse` struct in `daemon.capnp`;
  Go bindings regenerated.
- Tests in `version_handshake_blackbox_test.rs`: shape contract,
  idempotency, works-before-other-ops.

### Added — self-maintaining `compatibility.json` (ley-line-open-cbea02)

- New tool `rs/tools/compat-gen/` reads the `version.rs` constants
  and emits `compatibility.json` to stdout. Committed at the repo
  root; `task compat:check` is the CI gate that fails on drift.
- No hand-maintained version matrix. Updating any compat fact is
  exactly one edit in `version.rs`; the live `leyline_version` op
  AND the committed compat artifact both follow automatically.
- Doc `$schema_version: 1` field gates the artifact's own shape
  so future consumers parsing it can detect doc-version drift.

### Added — typed JSON event payload structs (ley-line-open-503971)

- New Go sub-package `clients/go/leyline-schema/daemon/wire/`
  with typed op-response structs (promoted from the previously
  test-internal types in `daemon_protocol_test.go`) and per-topic
  event-payload types: `Event` envelope plus
  `SheafInvalidatePayload`, `SheafTopologyPayload` (covers both
  set-topology and update-topology variants via the `Kind`
  discriminator), `DaemonSnapshotPayload`, `DaemonHeadChanged`,
  `DaemonFilesChanged`, `DaemonReparseComplete`, `DaemonOp`.
- `DecodeEvent(b []byte) (Event, any, error)` — one call to
  dispatch + decode; returns `json.RawMessage` for unknown topics
  so forward-compat is structural.
- Closes the silent-coercion bug class mache hit with hand-rolled
  `parseUint64`. With typed `json.Unmarshal` + `,string` tags,
  malformed input is a typed error the caller sees.
- Schema-client release tag `clients/go/leyline-schema/v0.4.5`
  follows this binary release.

### Added — HCL / Terraform parse support

- `leyline-ts` adds an `hcl` feature gating `tree-sitter-hcl = "1"`.
- `TsLanguage::Hcl` variant + aliases on every dispatch path:
  `from_name` accepts `hcl` / `terraform` / `tf` / `tfvars`
  (case-insensitive); `from_extension` accepts `.tf` / `.tfvars`
  / `.hcl` (case-insensitive).
- Daemon picks up the new language via cli-lib's feature pull —
  no per-consumer enablement needed.
- One grammar covers HCL + Terraform + Nomad + Vault + Packer +
  Consul Template per upstream's coverage.
- Tests: alias-set pins on extension + name paths, plus an
  end-to-end parse of a real Terraform fragment (terraform block
  + variable + resource).

## [0.4.4] — 2026-05-19

First tagged release since v0.4.2. Rolls up the event-bus correctness
work (5caa59 live-push, cb12fa replay-filter, u64 encoding), the
workspace dep audit (rusqlite + others), the unstructured-text
retrieval surface (text_search trait + WitchcraftEngine, opt-in
feature), and the observation-lattice ADR.

> **Note**: a paper v0.4.3 cut shipped in CHANGELOG + workspace
> `Cargo.toml` via PR #34 (`74a1ec5`) but was never tagged in git.
> v0.4.4 supersedes it; the rolled-up entry below preserves the v0.4.3
> content and adds the post-v0.4.3 changes.

**For consumers** (mache, cloister, future LLO clients): the daemon
now actually forwards live pushed events to UDS subscribers AND
filters the replay batch by topic. Pre-v0.4.4 daemons leaked unrelated
events to topic-scoped subscribers via the replay path (cb12fa);
pre-v0.4.3 daemons silently dropped all live-pushed events because
`event_rx` was never drained (5caa59). Mache's PR #384 e2e
(`TestE2E_SheafSubscriber_AgainstLiveDaemon`) flips RED→GREEN against
a v0.4.4 binary with no mache-side code change.

### Fixed — EventLog replay topic-filter leak (ley-line-open-cb12fa)

- `rs/ll-open/cli-lib/src/daemon/events.rs` — `EventRouter::subscribe`
  returned the EventLog slice (`log.since(since)`) as the replay batch
  attached to the subscribe response, without ever applying the new
  subscriber's topic-pattern filter. The live-dispatch path correctly
  invokes `Subscriber::matches()` per event in `assign_and_dispatch`;
  the replay path skipped that filter entirely.
- Symptom: a subscriber on `topics: ["sheaf.invalidate"]` received
  `daemon.snapshot` and any other pre-subscribe event sitting in the
  log — the replay batch was a topic-blind firehose. Surfaced by the
  mache evolve-coverage-trunk campaign + the sheaf-subscribe probe.
- Fix: new helper `EventLog::since_matching(since, predicate)` runs
  the topic-pattern filter inside the iterator (so only matching
  events are cloned out of the log); `EventRouter::subscribe` calls
  it with `|ev| sub.matches(&ev.topic, &ev.source)`. Behaviour now
  symmetric between replay and live-push.
- Regression guard:
  `event_push_blackbox_test.rs::subscribe_filters_events_by_topic_for_replay_and_live_push`
  asserts both replay and live-push only deliver subscribed topics —
  pre-fix the test fails at `replay_count == 1`; post-fix all five
  blackbox tests pass.

### Added — TextSearchEngine trait + Witchcraft engine + op_text_search (PR #31)

- New crate `leyline-text-search` (`rs/ll-open/text-search/`)
  introducing the `TextSearchEngine` trait — an unstructured-text
  retrieval surface alongside the existing single-vector `vec_search`.
- Default `NullEngine` returns `NotImplemented` from every op so the
  daemon op surface compiles and clients see a structured error
  rather than "unknown op" when no real backend is wired.
- `WitchcraftEngine` (feature `engine-witchcraft`, opt-in) wraps the
  upstream `witchcraft` crate's XTR-WARP late-interaction + BM25
  hybrid retrieval. Off by default; private extensions install the
  engine via `DaemonExt::text_search_engine()`.
- New wire op `BaseRequest::TextSearch { query, k }` + `op_text_search`
  handler + registration in both `base_op_names()` and the MCP tool
  registry, with a drift test cross-checking the two lists.
- Black-box gates: four `event_push_blackbox_test.rs` tests landed
  alongside; substrate-non-leak gate over `storage_path()` for every
  engine impl; count-contract tests pinning `len()` exactness across
  open-from-existing-DB, repeated upsert, remove-of-absent.

### Added — ADR-0020 observation flow over a learned CellComplex (PR #35)

- `docs/adr/0020-entity-observation-lattice.md` proposes the
  substrate model for LLO's master-DB direction: one `observation`
  table whose rows reference observer-emitted mention tokens, with a
  periodic `ComplexBuildPass` building a `CellComplex` from mention
  co-occurrence and `CoChangeTracker` learning edge weights from the
  temporal flow. Three query primitives (`neighborhood`, `agreement`,
  `co_changed_with`) replace the prescriptive lens APIs an earlier
  draft proposed.
- Math layer is load-bearing: Gate 2 fails the build if the code
  path doesn't mechanically invoke `leyline-sheaf::CellComplex` —
  proves the substrate honors its math citation rather than dressing
  up the schema with vocabulary.

### Fixed — sheaf.invalidate event push over UDS (ley-line-open-5caa59)

- `rs/ll-open/cli-lib/src/daemon/socket.rs` — `handle_connection` was
  a pure request/response loop that never drained
  `ConnectionState.event_rx`. Live events emitted onto the bus after
  subscribe were `try_send`'d into the per-subscriber `mpsc::Sender`
  and accumulated with no consumer. The bug was masked by the
  EventLog replay batch appended to the subscribe response.
- Fix: per-connection writer task (sole owner of `OwnedWriteHalf`,
  drains a bounded `mpsc<String>` so op responses and events serialise
  without interleaving) plus a per-subscribe event-relay task
  (forwards events from `event_rx` as JSON lines through the writer
  channel). Resubscribe replaces the relay cleanly via the existing
  `remove_subscriber` → `Subscriber.tx`-drop → `recv() = None` chain.
- Regression guard:
  `rs/ll-open/cli-lib/tests/event_push_blackbox_test.rs` — four
  black-box tests over real UDS sockets pinning the core push path,
  emit ordering, resubscribe replacement, and the bead's exact
  `sheaf.invalidate` reproduction. All subscribe BEFORE any emit so
  none rely on replay.
- Verified end-to-end against mache's
  `tools/sheaf-subscribe-probe/main.go`: `sheaf.invalidate event
  received in 19.083µs`.
- Originally shipped as PR #32, included in the paper v0.4.3 cut.

### Fixed — event payload u64 encoding (surfaced during 5caa59 validation)

- Event payloads emitted u64 fields as raw JSON numbers
  (`"generation": 1`) while the matching capnp op responses emitted
  them as quoted strings (`"generation": "1"` — capnp_json's
  convention to dodge JS Number's 2^53 safe-integer ceiling). Consumers
  reading both surfaces (mache's `SheafSubscriber`, cloister's event
  consumers) had to handle two encodings for the same field.
- Fix: stringify u64 fields at the emit site to match capnp_json. Three
  events touched — `sheaf.invalidate` (`generation`,
  `prior_generation`), `sheaf.topology` from `sheaf_update_topology`
  (same), `daemon.reparse.complete` (`parsed`, `deleted`).
- Pinned in `tests/event_push_blackbox_test.rs::sheaf_invalidate_event_reaches_uds_subscriber`
  — asserts both u64 fields are `is_string()` on the wire so a future
  regression to raw numbers fails the gate.

### Changed — workspace dep audit (Witchcraft unblock)

- `rusqlite 0.34 → 0.39` across 8 workspace `Cargo.toml`s. Migration:
  `DatabaseName::Main` → `"main"` (new `Name` trait with blanket
  `&str` impl); `usize`/`u64` SQLite binds and reads cast through
  `i64` (the `ToSql`/`FromSql` blanket impls were dropped). Unblocks
  the optional Witchcraft retrieval feature, which pins `rusqlite
  ^0.39` (`libsqlite3-sys`'s `links = "sqlite3"` allows exactly one
  version per dep graph).
- `dirs 5 → 6` · `which 7 → 8` · `criterion 0.5 → 0.8` (dev) ·
  `lsp-types 0.95 → 0.97` (`Url` → `Uri` newtype around
  `fluent_uri::Uri<String>`). Picked up via `cargo update` (wildcard
  pins): `tokio 1.50 → 1.52` · `clap 4.6.0 → 4.6.1` · `libc 0.2.183
  → 0.2.186` · `sqlite-vec 0.1.7 → 0.1.9` · `tree-sitter 0.26.7 →
  0.26.8`. `capnp =0.25.0` deliberately stays at the ADR-0014 §3
  toolchain triplet — bumping requires the F8.6.4 cross-runtime
  fixture regen, deferred to a dedicated PR.

### Deferred to follow-up PRs

`thiserror 1 → 2`, `nalgebra 0.32 → 0.34` (coupled to sheaf math),
`sha2 0.10 → 0.11`, `der`/`spki`/`const-oid` (signing pipeline),
`jj-lib 0.38 → 0.41`, `fuser 0.15 → 0.17`, `nfsserve 0.10 → 0.11`,
`uv-* 0.0.29 → 0.0.47`, `toml 0.8 → 1.0` — each warrants its own
migration + test review.

## [0.4.2] — 2026-05-17

Feature release shipping the sheaf-driven cache-coherence GC trilogy
plus a CI-enforced cold-parse perf gate and a release-workflow fix.

**For consumers** (mache, cloister, future LLO clients): with v0.4.2
the sheaf surface is structurally complete — `set_topology` to seed,
`update_topology` for incremental deltas, `invalidate` for asserted
changes, `reap` for observational eviction, and `prior_generation`
continuity tags so a consumer can detect missed events between two
generations. The file-system-style "payload-blind GC where the trigger
is structural, not content" idiom is operational.

No public API change from v0.4.1 — all sheaf wire fields are purely
additive (priorGeneration on two responses, new SheafReap{Request,
Response}, new TopologyDelta + UpdateTopology{Request,Response}).
Existing v0.4.1 consumers ignoring unknown fields keep working.

This release should be the first to ship **4 binaries** (linux + macos
× amd64 + arm64) — v0.4.0/v0.4.1 shipped 3-of-4 because the macos-13
runner was perma-queued. PR #23 fixed the matrix to cross-compile
darwin-amd64 from macos-latest.

### Added — sheaf_update_topology (incremental delta op, GC item 2)

- New `op_sheaf_update_topology` (`rs/ll-open/cli-lib/src/daemon/
  sheaf_ops.rs`). Today's `sheaf_set_topology` replaces the whole
  `CellComplex` atomically; this op applies a delta (added/removed
  regions, edge changes, stalk updates) and preserves cached entries
  for untouched regions. Returns `affected_regions` (touched ∪
  radius-1 BFS neighbours) so consumers know exactly which keys to
  evict — every region outside this set is byte-identical to its
  pre-update value.
- New `CellComplex::apply_delta`, `remove_node`, `remove_edge` in
  `sheaf/src/complex.rs` plus `SheafCache::refresh_baseline_subset`,
  `drop_region`, `drop_restriction`, `neighbours`, `complex_mut`,
  `bump_generation` in `sheaf/src/cache.rs`.
- Wire: `TopologyDelta`, `EdgeRef`, `StalkUpdate`,
  `SheafUpdateTopologyRequest`, `SheafUpdateTopologyResponse` in
  `daemon.capnp`. Go bindings regen'd.
- 4 new falsifiability gates (`incremental_update_preserves_untouched_
  cache_entries`, `affected_regions_includes_radius_1_neighbours`,
  `add_region_baseline_matches_set_topology`, `concurrent_updates_
  serialize_correctly`) + 1 black-box UDS gate (`update_topology_over_
  uds_returns_affected_subset_not_whole_graph`). Tracked by bead
  `ley-line-open-9d2302`; merged via PR #25.

### Added — sheaf_reap (δ⁰-driven GC op, GC item 3)

- New `op_sheaf_reap` (`sheaf_ops.rs`). Pure observational query:
  "given today's stalks vs the last baseline, which cached region IDs
  can the consumer safely evict?". Returns `reclaimable`, `count`,
  `generation`, `reaped_at_defect`. Read-only — does NOT bump
  generation so consumers can call repeatedly during one enrichment
  pass without advancing their cursor.
- New `SheafCache::reap` in `sheaf/src/cache.rs`. Walks restriction
  edges, finds those whose ‖δ⁰‖² has moved beyond `DELTA0_EPS_SQUARED`
  from baseline, BFS-expands to radius 3 (same depth as `on_change`).
  Payload-blind by construction — never inspects the consumer's
  cached `V`. NaN defect when no `CellComplex` attached.
- Wire: `SheafReapRequest`, `SheafReapResponse` in `daemon.capnp`.
- 4 new falsifiability gates (`reap_no_false_positives_on_unchanged_
  stalks`, `reap_no_false_negatives_when_stalks_move`, `reap_payload_
  blind_under_different_v_types`, `reap_returns_empty_and_nan_without_
  complex`) + 1 black-box UDS gate (`sheaf_reap_observes_drift_over_
  uds`). Tracked by bead `ley-line-open-9c867f`; merged via PR #26.

### Added — prior_generation continuity tag (GC item 1)

- New `priorGeneration` field on `SheafInvalidateResponse` and
  `SheafUpdateTopologyResponse`. Carries the generation value
  immediately before the op bumped it; consumers verify `their_last_
  seen == response.prior_generation` to detect missed events between
  two generations.
- Intentionally NOT added to `SheafReapResponse` since reap doesn't
  bump generation — `prior_generation == generation` would be useless
  info. Scope trimmed from the bead after implementation insight.
- 2 new black-box UDS gates (`sheaf_invalidate_prior_generation_
  continuity_over_uds`, `sheaf_update_topology_prior_generation_
  continuity_over_uds`). Pins monotonicity + first-call-prior-is-zero
  + cross-op continuity (invalidate → update sequence). Tracked by
  bead `ley-line-open-9d5d7d`; merged via PR #27.

### Added — cold-parse perf regression gate

- `rs/ll-open/cli-lib/tests/cold_parse_perf_regression.rs` —
  synthesizes a deterministic 800-file Go corpus from committed
  fixtures, runs `cmd_parse`, asserts `wall < 500ms` AND `per_row <
  25us`. Per-row budget is the adaptive assertion — catches un-batched
  insert regressions even when corpus shape drifts.
- Gated behind `LLO_PERF_GATES=1` (same convention as `topology_pass_
  test.rs`); `task ci` sets the env via a new `task test:perf` step.
  Plain `cargo test` skips. Per user feedback ("CI is kinda ass in
  GHA lol"), enforcement lives in `task ci` (local pre-push) not GHA.
- Tracked by bead `ley-line-open-a3f254`; merged via PR #24.

### Fixed — release workflow macos-13 perma-queue

- `.github/workflows/release.yml` — `build leyline-darwin-amd64`
  switched from `os: macos-13` to `os: macos-latest` (arm64) with
  `target: x86_64-apple-darwin` cross-compile. The macos-13 hosted
  runner pool was perma-oversubscribed; v0.4.0 and v0.4.1 both saw
  this job sit in `status=queued` indefinitely (never executed),
  shipping 3-of-4 binaries per release. Apple's clang on arm64 macOS
  handles both archs natively. This is v0.4.2's first true 4-binary
  release if the workflow holds. Tracked by bead `ley-line-open-
  392bd7`; merged via PR #23.

## [0.4.1] — 2026-05-17

Patch release. Ships the P0 wire fix that unblocks mache δ⁰ adoption,
a topology pre-pass module that materialises the inputs the future
`sheaf_update_topology` op will consume, a cold-parse perf drill
(5040ms → ~1475ms median wall on a 766-file mache-sized repo), and
two ADRs (0015 lazy-on-access ingestion, 0016 AI-native query surface)
that frame the v0.5 design space.

No public API change relative to v0.4.0; wire shape is identical for
all in-tree ops. Consumers on v0.4.0 should upgrade — the cascade fix
is observable.

### Fixed

- **P0**: `sheaf_invalidate` over UDS now returns the changed roots
  the caller passed in *plus* any δ⁰ / XOR-trigger neighbours, instead
  of an empty array. Prior v0.4.0 release shipped with the cascade
  gated on the daemon's local `SheafCache::entries` map being
  populated — but no daemon op populates `entries`, so UDS / MCP
  consumers (mache included) observed `invalidated: []` for every
  call. Decouples cascade output from cache contents; in-process
  callers see the same answer UDS consumers do. Black-box regression
  test in `cli-lib/tests/sheaf_uds_blackbox_test.rs` (2 tests, both
  modes) and falsifiability gate
  `claim_2c_changed_roots_are_returned_even_when_entries_are_empty`
  pin the contract. Tracked by bead `ley-line-open-d03e7d`; merged
  via PR #19.

### Added — `topology_pass` module

- `rs/ll-open/cli-lib/src/topology_pass.rs` (~1100 LOC) — pre-parse
  pass that walks the workspace, scans for `Cargo.toml` / `go.mod` /
  `pyproject.toml` / `package.json` manifests, sweeps regex
  imports across 4 languages (Rust `use`/`pub use`, Go single + grouped,
  Python `import`/`from`, TypeScript `import`/`export-from` with
  token-boundary `import()`-detection and comment-skip), clusters
  regions by manifest ancestor (`BTreeMap` ancestor-walk, O(n log m)),
  and emits a `TopologyOutput` with the `region_edges` that translate
  into `SheafRestrictionInput` for the future
  `sheaf_update_topology` op. 11 falsifiability gates
  (`tests/topology_pass_test.rs`): empty-file cost lower bound,
  realistic-size cost ceiling, scaling claim, 4-language presence,
  root + subcrate / depth-3 / bloat-dir scenarios, determinism, and
  region-edge → sheaf-input translation spot-check. Gated behind
  `LLO_PERF_GATES` env var. Skeptic-cleared (5 important + 4 nit
  findings addressed) and Copilot-cleared (3 findings addressed).
  Tracked by bead `ley-line-open-9d3208`; merged via PR #20.

### Performance — cold-parse wall

- `rs/ll-open/cli-lib/src/cmd_parse.rs` — three-attack drill on
  the 5040ms baseline for `leyline parse ~mache repo` (766 files,
  535k AST nodes):
  - **Insert phase**: batched VALUES inserts (`BULK_BATCH_ROWS = 3000`
    × 9 columns = 27000 params, under the 32766 SQLite parameter
    cap; ~60 KB per statement). `BufWriter` wraps the capnp dual-
    write path to coalesce syscalls. Indexes deferred until after
    `COMMIT`.
  - **Head-write**: parallel head-write thread runs alongside insert
    instead of after. Cold parse skips the sweep step entirely
    (`sweep_orphaned_dirs` only runs when prior generation existed,
    falsifiability gate
    `sweep_orphaned_dirs_runs_when_files_are_deleted_between_parses`
    pins this). On cold parse: `head_write=0ms sweep_close=0ms`.
  - **Process exit**: `libc::_exit(0)` from `main()` (CLI-only, gated
    on `is_parse && r.is_ok()`) plus `mem::forget(conn)` to avoid
    sqlite's destructor flush — the OS reclaims pages faster than
    the Drop chain. NOT in the daemon path.
  - **Result**: median wall **1475ms** (3 release runs on
    `~/github/art/mache`: 1531ms / 1474ms / 1475ms). 70% reduction.
    Insert phase now dominates (~92% of wall); future drilling
    targets prepared-statement batching or mmap'd staging.
  - Falsifiability gate
    `batched_inserts_preserve_record_content_not_just_row_count`
    verifies the batch optimisation doesn't drop rows.
  - Skeptic-cleared (5 findings addressed including doc accuracy,
    test of the deferred sweep path, lint cleanup, fmt diff).
  - Tracked by bead `ley-line-open-cbbedf`; merged via PR #21.

### Added — ADRs

- `docs/adr/0015-lazy-on-access-ingestion.md` (220 lines) — 7
  decisions × ≥2 alternatives + falsifiability each. POSIX syscall
  partition (read / mmap trigger; stat / access / readdir don't),
  FSEvents vs kqueue, `SheafCache` as on-miss backing. Forwards
  consumer-shape question to ADR-0016.
- `docs/adr/0016-ai-native-query-surface.md` (410 lines) — 8
  decisions × 3 alternatives + falsifiability each. Symbol-keyed,
  bundled, structured, stateless protocol. Worked example: 1
  round-trip / 4.3 KB vs LSP's 12 / 9.8 KB. LSP 3.17 coverage map:
  41 methods placed (14 supported / 13 deferred / 14 unsupported).
- Tracked by beads `ley-line-open-9db858` (0015) and
  `ley-line-open-9f491f` (0016); merged via PR #18.

## [0.4.0] — 2026-05-14

Feature release: lifts the leyline-sheaf Čech cohomology engine from
the private `ley-line` repo into LLO as a first-class OSS crate, wires
it through the daemon's UDS + MCP surfaces (6 ops), and ships the
δ⁰-driven cache invalidation contract as the load-bearing moat claim.
Tracked by bead `ley-line-open-ae7a35`; merged via PR #16.

Paired Go bindings tag: `clients/go/leyline-schema/v0.4.0`. mache
adoption tracked under beads `mache-8e2e92` (typed bindings),
`mache-8e59a5` (δ⁰ mode opt-in), `mache-8e7794` (cross-runtime test).

### Added — new crate `leyline-sheaf`

- `rs/ll-open/sheaf/` — 7 modules (`cache`, `complex`, `learn`,
  `merkle`, `sparse`, `topology`, `lib`), ~2,300 LOC. Domain-
  independent Čech cochain complex with δ⁰/δ¹ coboundary operators,
  per-edge defect (`‖δ⁰‖²`) computation, restriction-map composition,
  SHA-256 Merkle root, sparse matrix ops, and a `SheafCache<S, V>`
  with bounded-BFS invalidation cascade.
- `SheafCache::with_complex(cx)` / `set_complex(cx)` opt-in to
  δ⁰-driven invalidation; `refresh_baseline()` snapshots the current
  per-edge `‖δ⁰‖²` so subsequent `on_change` compares *change* in
  defect, not absolute. Heuristic XOR-Merkle path remains the default.
- `RestrictionMap::project_dim_range(stalk_dim, agreement_dim)` — the
  canonical "shared contract subspace" selector used by both the
  daemon wire-side `agreement_dim` shorthand and the real-repo bench.
- 48 unit tests + 8 falsifiability gates (`tests/falsifiability_
  gates.rs`) pin the math contract: `defect == ‖δ⁰‖²`, BFS
  cascade contract, δ⁰ keeps neighbor valid when projection-image
  unchanged.
- Real-repo bench (`tests/real_repo_sheaf_bench.rs`) builds the sheaf
  over this crate's own 7 source files (parser-derived `use crate::*`
  import edges) and measures simulated parse-time. Result: δ⁰ saves
  **66% parse time** on projected-away noise (3448 µs vs 10108 µs).

### Added — daemon ops (UDS + MCP)

Six new ops over the typed `BaseRequest` dispatch in
`rs/ll-open/cli-lib/src/daemon/sheaf_ops.rs`:

- `sheaf_set_topology` — push regions (with optional f32 stalk
  `data`) + restriction edges (with optional `agreement_dim`) + the
  request's `node_stalk_dim`. When the opt-in conditions are met,
  the handler builds a backing `CellComplex` with implicit
  `project_dim_range` restriction maps, runs `refresh_baseline()`,
  and the response advertises `delta_zero_mode: true`.
- `sheaf_invalidate` — report changed region ids (optionally with
  new stalk hashes + f32 stalk data); runs the bounded BFS cascade
  and returns the invalidated set + cache generation. f32 stalk
  updates push into the complex via `set_stalk_value`.
- `sheaf_defect` — total `Σ‖δ⁰‖²` + cache validity counts.
- `sheaf_stalks` — `generation`, `valid`, `total`.
- `sheaf_status` — combined health snapshot (+ `tracked_edges`).
- `sheaf_learned_weights` — co-change-derived per-edge coupling rates
  from the `CoChangeTracker`.

`SheafState` lives on `DaemonContext` with cache + tracker + event-bus
emitter. Topology + invalidation events emit on the ADR-010 bus
(`sheaf.topology` / `sheaf.invalidate`).

### Added — capnp schema (additive per ADR-0014 §2)

`rs/ll-core/public-schema/capnp/daemon.capnp`:

- `SheafStalk` (with optional `data :List(Float32)`)
- `SheafRestriction` (with optional `agreementDim :UInt32`)
- `SheafSetTopologyRequest` (with `nodeStalkDim`)
- `SheafSetTopologyResponse` (with `deltaZeroMode`)
- `SheafInvalidateRequest`, `SheafInvalidateResponse`
- `SheafDefectResponse`, `SheafStalksResponse`, `SheafStatusResponse`
- `SheafLearnedWeight`, `SheafLearnedWeightsResponse`

Go bindings regenerated; cross-runtime drift gate
(`clients/go/leyline-schema/daemon/daemon_protocol_test.go`) covers
all 6 ops.

### Performance

- Workspace release profile bumped to `lto = "thin"` +
  `codegen-units = 1` so per-edge δ⁰ inlines through nalgebra's
  matrix indexing and the autovectorizer sees contiguous f32 ops
  across the crate boundary.
- `CellComplex::edge_violation_squared` rewritten as an alloc-free
  column-major sweep over the raw restriction-matrix slices;
  `#[inline]` so the cache hot path doesn't pay function-call cost.

### Fixed — math correctness (from the math-friend audit on PR #16)

- `detect_violations` asymmetric check (only flagged negative-margin
  δ⁰): now symmetric on `|val| > EPS`.
- `add_edge` accepting wrong-shape restriction maps: now asserts
  `ncols == node_stalk_dim` and `nrows == agreement_dim`.
- `enforce_transitive_closure` cloning unrelated defaults: now
  composes the actual stored maps along the path via
  `compose_path_maps`; paths whose composition doesn't fit the
  requested agreement dim are skipped (no silent fallback).
- δ¹ orientation hardcoded `+1.0`: now records ±1 signs via
  `signed_lookup` so cycles traversing edges against their natural
  direction contribute correctly. `add_face` panics on garbage
  (empty edges, duplicates, unknown ids, non-cycle).
- `compute_h0` was misnamed (returned a section-dependent partition
  not the cohomology group): renamed to `consistency_analysis`;
  `h0_dimension` documented as the canonical `dim ker(δ⁰)`.
- `compute_merkle_root([])` returned all-zeros (collision risk): now
  returns `H(0x02 || "empty")`, domain-separated.
- HashMap → BTreeMap throughout the cache + complex iteration paths
  for deterministic ordering. Cascade is now genuine BFS (VecDeque +
  pop_front), not the DFS that `Vec::pop` produced.

### Bumped — workspace crate versions

All workspace crates: `0.3.0` → `0.4.0` (and `leyline-sheaf`:
`0.1.0` → `0.4.0` for uniformity, matching the v0.3.0 sync). OCI
image tag: `ley-line-open:0.3.0` → `ley-line-open:0.4.0`.

## [0.3.0] — 2026-05-12

Coordinated breaking release. The daemon UDS + MCP JSON wire migrated
to the capnp-json codec (C++ JsonCodec compatible) via the
`daemon-typed-wire` decade (beads `b5a77b` / `b631c8` / `b69606` /
`b0ea2e`). `daemon.capnp` is now the load-bearing typed contract —
every base-op response flows through capnp builders + `capnp_json::to_json`.

Paired Go bindings tag: `clients/go/leyline-schema/v0.3.0`. mache
adoption tracked under bead `mache-a5ad09`.

**Crate versions bumped uniformly** from `0.1.0` → `0.3.0` across the
workspace. The previous tags (`v0.1.0`, `v0.1.1`, `v0.2.0`) had drifted
from the in-tree Cargo.toml; this release synchronizes them.

### Breaking — wire-shape changes

- **Int64 / UInt64 fields emit as JSON strings.** `"67108864"` not
  `67108864`. C++ JsonCodec convention (avoids JS Number precision
  loss). Inbound (consumer→LLO) still accepts both string and raw-
  number forms; only the outbound direction breaks.
- **Defaulted Int fields always emit `"0"`** because capnp Int defaults
  to 0 and capnp-json has no skip-if-default annotation. `generation`
  reappears as `"0"` on every base-op response (legacy ordinal kept per
  ADR-0014 §2; semantically dead — `current_root` is the canonical
  identity). `last_reparse_at_ms` appears as `"0"` pre-first-reparse.
- **`StatusResponse.enrichment` legacy Text field no longer emitted.**
  Reshaped into typed `enrichment_typed: List(EnrichmentEntry)` —
  each entry is `{name, status: PassStatus { last_run_at_ms, basis,
  error }}`. No double-parse on consumers. Legacy ordinal `@6 :Text`
  stays in the schema per ADR-0014 §2 but the handler leaves it unset
  (capnp-json omits unset Text on the wire).
- **`ErrorResponse` gained `ok @1 :Bool`** (additive) so the canonical
  `{"ok": false, "error": "..."}` envelope keeps shape end-to-end.

### Added

- **A-1 / `ley-line-open-b5a77b`** (#6, 2026-05-11): cross-runtime
  fixture gate for the daemon JSON protocol. Single fixture file
  `rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` pins each
  op's request shape, response shape, and required-key set. Consumed
  by a Rust integration test (handler-output) and a Go test
  (typed-decode under strict-unmarshal). Drift between schema,
  handler, and wire is now a CI failure.
- **A-2 / `ley-line-open-b631c8`** (#8 first half, 2026-05-11):
  schema↔reality reconciliation. `daemon.capnp` extended additively
  per ADR-0014 §2 with the fields the handlers were already emitting
  (StatusResponse gained `phase`, `currentRoot`, `enrichment`,
  `headSha`, `lastReparseAtMs`, `error`; ReparseResponse gained the
  flat stats; SnapshotResponse / LoadResponse / EnrichResponse each
  gained `currentRoot`; new FlushRequest/Response,
  FindCalleesRequest/Response, TokenMapEntry, GetRefsMap/Response,
  GetDefsMap/Response, SchemaTier, GetSchema/Response, GetDbPath/Response).
- **A-3 / `ley-line-open-b69606`** (#8 second half, 2026-05-11): typed
  serde response mirror (`daemon/wire.rs`) — `BaseRequest` tagged enum
  + `dispatch_typed` exhaustive match — covering all 23 base ops.
- **b0ea2e / capnp-json adoption** (#11 + #12, 2026-05-12): the
  capnp-json codec is now the response-side encoder; `wire.rs` shrunk
  from 444 → 178 lines (request enum stays; response structs deleted).
  Schema is the load-bearing contract.
- **Five new daemon ops** that mache needed (#7, 2026-05-11):
  `find_callees`, `get_refs_map`, `get_defs_map`, `get_schema`,
  `get_db_path`. Wired through both UDS and MCP HTTP transports.
- **OCI image `ley-line-open:0.3.0`** — distroless, ~20 MB, built via
  krust + cargo-zigbuild. Default CMD `daemon --mcp-port 8384
  --mcp-bind 0.0.0.0`. Previous image label `0.2.1` is superseded.
- **Five new per-crate READMEs** for the previously-undocumented crates
  (`rs/`, `cli-lib`, `cli`, `public-schema`, `hdc`). Format matches the
  existing 8: tagline + "What's here" bullets, no fluff.

### Changed

- **`op_list_children` no longer SELECTs `nodes.record`** (Copilot
  review on PR #8). Directory listings could carry full file contents
  of every child (megabytes per row in some repos). `record` is now
  `Option<String>` with `skip_serializing_if`; listings omit the field.
  Consumers needing record call `op_get_node` or `op_read_content`.
- **Workspace capnp toolchain bumped** to `=0.25.0` (exact-pin per
  ADR-0014 §3) to enable the capnp-json runtime codec.
- **Crate versions** in every `rs/ll-core/*/Cargo.toml` and
  `rs/ll-open/*/Cargo.toml` bumped from `0.1.0` to `0.3.0` to match
  the git tag. Synchronizes in-tree version metadata with the tag
  history that had drifted since `v0.1.0`.

### Notes

- LLO daemon binary tag is now `v0.3.0` — matching the workspace
  Cargo.toml versions, the image label, and the Go bindings module
  tag. One coherent SemVer line going forward.
- mache + cloister + any other Go consumer needs to adopt
  `clients/go/leyline-schema/v0.3.0` with `,string` json tags on
  `*int64`/`*uint64` fields. See `mache-a5ad09` for the canonical
  Go struct definitions.
- Bead `40df83` (dual-codec binary capnp + JSON via magic-byte
  dispatch) is the natural step 4 — when a consumer asks for typed
  end-to-end on the wire (skipping JSON entirely), that's the path.

## [0.2.0] — 2026-05-09

Coordinated breaking release with [mache v0.8.0](https://github.com/agentic-research/mache/releases/tag/v0.8.0)
(paired via mache PR #365 + #366). Cutover wave to a content-addressed
Σ substrate. **Old binaries and new binaries are incompatible by
design.** A v0.1.x binary opening a v0.2.0 control or arena file (or
vice versa) hits an explicit VERSION-mismatch error rather than
silently misreading the byte layout.

### Added
- **Σ substrate type surface** — bead `ley-line-open-9e3a5f`. New
  module `leyline_core::substrate` declaring `Hash`,
  `ContentAddressed`, `BlobStore`, `RootPointer`, `RootSigner`. No
  behavior — implementations land in subsequent threads.
- **`Controller::current_root()`** as the substrate's primary public
  identity field — bead `ley-line-open-baa90a`. Returns the 32-byte
  BLAKE3 root of the active arena payload.
- **`Controller::set_arena_with_root(path, size, root)`** — bead
  `ley-line-open-babf6a`. Atomic publish of `(path, size, root)`
  under a single Release-store on a private sync counter.
- **`ArenaHeader.data_size: u64`** — exact length of the live
  payload in the active buffer. Lets readers hash `buf[..data_size]`
  without parsing format-specific headers (replaces the unreliable
  SQLite `page_count` parse path).
- **Reader-side σ verification before deserialize** — bead
  `ley-line-open-bad8f1`. `SqliteGraph::from_arena` and
  `SqliteGraphAdapter::from_arena_writable` BLAKE3-hash the buffer
  prefix and refuse to load on mismatch.
- **`HotSwapGraph` polls `current_root`** — was `current_gen`.
  Idempotent root semantics: re-snapshot of unchanged db produces
  the same root, no spurious swap.
- **`leyline-vcs` and `leyline-sign` crates** — bead
  `ley-line-open-889173`. Lifted from the (private) ley-line repo
  with license bump Apache → AGPL with NOTICE. `leyline-vcs` is
  migrated to root-polling; `leyline-sign` is unchanged behaviorally.
- **Static-assertions on control-block field alignment**
  (`OFF_GENERATION`, `OFF_ARENA_SIZE` must be 8-byte aligned).
  Compile fails rather than silently producing UB on architectures
  requiring naturally-aligned atomics if a future field reorder
  violates the invariant.
- **`ArenaHeader::validate_header(file_size) -> Result<u64,
  ArenaHeaderError>`** with a typed `ArenaHeaderError` enum so
  callers surface `VersionMismatch` distinctly from `BadMagic`,
  `BadActiveBuffer`, or `TruncatedFile` without parsing error
  strings.
- New tests pinning the closed downgrade hole
  (`t24_reader_rejects_zero_root_with_data`) and the legitimate
  fresh-arena path (`t24_reader_accepts_zero_root_with_empty_data`).

### Changed
- **Wire-format breaking change.** Every state-publishing daemon op
  (`status`, `flush`, `load`, `reparse`, `enrich`, `snapshot`)
  replaces `"generation": <u64>` with
  `"current_root": "<64-char-hex>"`. Fresh controllers emit the
  zero-sentinel `"0000…0000"`. Pin test enforces both presence of
  `current_root` and absence of legacy `generation`. Bead
  `ley-line-open-baee26`.
- **Controller `.ctrl` and ArenaHeader `.arena` VERSION 1 → 2.**
  `Controller::open_or_create` and `ArenaHeader::active_buffer_offset`
  reject the opposite version with a descriptive error pointing at
  the coordinated-cutover requirement.
- **`Controller::generation()` removed from the public API.** The
  byte slot is preserved as a private sync atom (`AtomicU64`) used
  only for Acquire/Release fencing. Callers cannot reach it; all
  polling is by root.
- **`bump_sync_counter_release` uses `fetch_add(1, Release)`** —
  was load-modify-store under a single-writer assumption that the
  file-backed mmap doesn't enforce across processes. The Release
  ordering still gives the happens-before pair with
  `sync_counter_acquire`; the atomic `fetch_add` closes the lost-
  increment window if the assumption is ever violated.
- **`set_arena(path, size)` is now 2-arg** (was 3 with a generation
  parameter). Re-advertises path/size without advancing the root.
- **`verify_arena_root` returns `&buf[..data_size]`** so the
  producer of "what got hashed" is the producer of "what gets
  deserialized." Eliminates the prior asymmetry where the verifier
  hashed a prefix but `sqlite3_deserialize` received the full
  padded buffer.
- **`ArenaHeader::buffer_size` saturates at 0** for files smaller
  than the 4096-byte header, instead of underflowing `u64` and
  producing a near-`u64::MAX` value that panicked on slice indexing.
- **`ArenaHeader::active_buffer_offset`** returns `None` for
  truncated files (`file_size < HEADER_SIZE`).
- **`cmd_daemon::warm_start_from_arena`** now uses `validate_header`
  → operators see "stale VERSION 1 arena, run the cutover"
  distinctly from "torn header" or "truncated file" rather than a
  generic "header invalid" warning.
- **`set_current_root` is `#[cfg(test)]`.** The unfenced direct-
  write setter was previously `pub` but documented as test-only;
  cfg-gating prevents production code from reaching the unfenced
  path even by accident.
- **License: Apache 2.0 → AGPL-3.0-only with NOTICE.** Applies to
  the lifted `vcs` and `sign` crates; the rest of LLO was already
  AGPL.
- Stale doc comments referencing the V1 `generation` API rewritten
  in the substrate's hot-path files: `control.rs`, `cmd_load.rs`,
  `cmd_daemon.rs`, `daemon/ops.rs`, `fs/graph.rs`,
  `lsp/src/project.rs`.
- Operator-facing strings (error messages, log warnings) scrubbed
  of T-shortcodes: `ArenaHeaderError::VersionMismatch` Display,
  `Controller::open_or_create` VERSION-mismatch bail, the
  `cmd_parse::write_head_after_parse` warn line. Substrate
  internals still use `T2.x` / `T8.x` shorthand in dev-facing
  comments paired with the canonical `ley-line-open-…` bead ID
  in adjacent context (full sweep deferred — comment churn).

### Removed
- Public `Controller::generation()` method.
- `"generation"` field from every daemon JSON response.
- `crates/vcs/` and `crates/sign/` from the (private) `ley-line`
  workspace — both moved to `rs/ll-open/{vcs,sign}/` here.
- Zero-root sentinel skip in `verify_arena_root` (see Security).

### Fixed
- BLAKE3-hash recovery from arena bytes no longer relies on parsing
  SQLite's in-header `page_count` (drifted from actual serialized
  byte count under WAL/freelist patterns). Replaced by
  `ArenaHeader.data_size`.
- `sign/src/ffi.rs`: defensive null-pointer checks before every
  `slice::from_raw_parts` call. Per the Rust safety contract,
  `from_raw_parts` requires non-null pointers even when `len == 0`;
  without these checks, a C caller passing `NULL` would invoke UB.
  Mirrors the pattern in `fs/src/lib.rs`.

### Security
- **Removed the zero-root sentinel skip in `verify_arena_root`.**
  Pre-cutover, when `controller.current_root() == [0; 32]`, the
  reader silently bypassed BLAKE3 verification under the rationale
  of "legacy compat with V1 writers." With this release's hard V2
  cutover, no legacy V2 writer can produce data without a published
  root, so the skip became a dead-but-active downgrade path: any
  process able to write 32 bytes to the control block could disable
  content verification while leaving arbitrary bytes in the arena
  buffer for `sqlite3_deserialize`. The new contract:
  - `data_size == 0` → fresh arena, accepted (nothing to verify).
  - `data_size > 0` and `current_root == [0; 32]` → rejected loudly.
  - `data_size > 0` and `current_root != [0; 32]` → BLAKE3 compare.
- `ArenaHeader::validate_header` surfaces `VersionMismatch` as a
  typed error so operators distinguish "stale arena, run the
  cutover" from "disk corruption."

### Migration

Producers and consumers must update simultaneously. There is no
forward-compatibility shim. The recommended migration order:

1. Stop all running v0.1.x daemons and consumers (mache).
2. Delete or archive existing `.ctrl` and `.arena` files; v0.1.x
   files cannot be read by v0.2.0 binaries.
3. Upgrade LLO and mache to v0.2.0.
4. Restart producers; they will create v2 control + arena files.
5. Restart consumers; they will read the new format.

If upgrading produces a startup error like
`control block VERSION mismatch: file has v1, this binary expects v2`,
that is the cutover working as intended — remove the stale file.

## [0.1.1] — pre-changelog

See `git log v0.1.0..v0.1.1` for the commit-level history. This
release predates the structured changelog.

## [0.1.0] — pre-changelog

Initial public release of ley-line-open as the OSS substrate for the
ley-line stack. See `git log v0.1.0` for the initial-commit set.

[Unreleased]: https://github.com/agentic-research/ley-line-open/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.3.0
[clients/go/leyline-schema/v0.3.0]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.3.0
[clients/go/leyline-schema/v0.2.3]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.3
[clients/go/leyline-schema/v0.2.2]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.2
[clients/go/leyline-schema/v0.2.1]: https://github.com/agentic-research/ley-line-open/releases/tag/clients%2Fgo%2Fleyline-schema%2Fv0.2.1
[0.2.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.2.0
[0.1.1]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.1
[0.1.0]: https://github.com/agentic-research/ley-line-open/releases/tag/v0.1.0
