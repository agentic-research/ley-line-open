# Changelog

All notable changes to ley-line-open are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project adheres to [Semantic Versioning](https://semver.org/).

Each entry references the bead ID(s) tracking the work in
[rsry](https://github.com/agentic-research/rosary) so the full design
context, scoping notes, and review history are recoverable.

## [Unreleased]

Nothing yet — post-v0.4.5 changes land here.

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
