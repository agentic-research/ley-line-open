# Audit: Sheaf-driven incremental invalidation — live or paper?

**Bead:** ley-line-open-a3f764
**Date:** 2026-07-08
**Auditor:** Claude
**Base commit:** `621d901` (main)

## TL;DR

The math primitives + UDS ops are live and tested, but the loop from a
source-file edit to a region-precise `sheaf.invalidate` event is **not**
closed inside LLO — the consumer (mache) has to observe file changes,
decide which regions moved, and call `op_sheaf_invalidate` itself. LLO
provides a math coprocessor for consumer-driven cascades, not an
autonomous file-change-to-region-invalidation engine.

Loosely: the loop is ~35% live. Links 1–2 (watcher → scoped reparse) are
solid; link 6 (event → mache eviction) is solid; links 3–5 (reparse →
re-encode → violation detect → `sheaf.invalidate` emit) have no code
path in the OSS repo.

## Method

- Read every code path in the bead's audit scope.
- For each link in the chain, located concrete call sites (file + line
  number) or documented their absence.
- Grepped for `daemon.sheaf.*` and `sheaf.*` emission points across the
  daemon crate. Grepped for callers of `op_sheaf_invalidate`,
  `detect_violations`, `on_files_changed`.
- Cross-checked mache's consumer (`~/remotes/art/mache/internal/leyline/`)
  to see what it actually does with the surface.
- No dynamic instrumentation — this is a source-only trace.

## The trace table

| # | Link | Status | Notes |
|---|---|---|---|
| 1 | Source change → file watcher fires | **live** | `git_watch_loop` polls every `GIT_POLL_INTERVAL` (2s), diffs dirty set via `git_dirty_files` + `git_head`. `cmd_daemon.rs:932-1075`, emits `daemon.head.changed` (`:997`) and `daemon.files.changed` (`:1015`). |
| 2 | Watcher → `parse_into_conn(scope=dirty)` | **live** | `cmd_daemon.rs:1027-1034` builds `dirty_vec` and passes as `Some(scope)` to `parse_into_conn`. `parse_into_conn` at `cmd_parse.rs:461-500` honors the scope (only stats/parses files in the set). Emits `daemon.reparse.complete` at `cmd_daemon.rs:1055`. |
| 3 | Re-parse → HDC / complex-build pass re-encodes affected nodes | **missing** | `git_watch_loop` never calls into the enrichment pipeline. `HdcEnrichmentPass` (`hdc_enrich.rs:97-260`) + `ComplexBuildPass` (`complex_build_pass.rs:221-290`) run only when a consumer invokes `op_enrich` (`daemon/ops.rs:584-634`). The dirty set is not plumbed through. |
| 4 | Re-encode → recompute restrictions / `detect_violations` | **missing (as a watcher-driven step)** | The math is reachable but not from the watcher path. `CellComplex::detect_violations` is called synchronously inside `op_agreement` (`daemon/ops.rs:2847`) — a wire op consumers hit per-request. `ComplexBuildPass` builds a `CellComplex` from `observation` rows on each run but drops it on function return; nothing writes back to `SheafState.cache`. See `complex_build_pass.rs:241-248` (writes set is empty; V1 stores complex + tracker in process memory only). |
| 5 | Violation set → `daemon.sheaf.*` invalidation event | **missing** | Zero emissions of `daemon.sheaf.*` topics anywhere in the source. `sheaf.topology` + `sheaf.invalidate` are emitted only inside `sheaf_ops.rs` handlers (`:273`, `:336`, `:659`), invoked by consumer UDS ops — not by the watcher, reparse, or any enrichment pass. |
| 6 | Invalidation event → mache cache flush via UDS | **live conditionally** | mache's `SheafSubscriber` (`sheaf_subscriber.go:340-370`) subscribes to `sheaf.invalidate`, parses the payload, and hands `(Invalidated, Generation)` to a handler. Pinned working since LLO v0.4.3 (bead `ley-line-open-5caa59`). But nothing in LLO drives the event from a file change — mache has to have called `op_sheaf_invalidate` itself. |
| 7 | mache → agent fresh answer | **live conditionally** | `SheafClient.InvalidateWithStalk` (`mache/internal/leyline/sheaf.go:146-175`) returns the daemon's BFS-ordered cascade list; consumer evicts locally-cached region entries. End-to-end observable via `sheaf_e2e_test.go` / `sheaf_subscriber_e2e_test.go`. But like link 6, this is consumer-driven, not source-change-driven. |

## Detailed findings

### Link 1: Source change → file watcher fires — **live**

`cmd_daemon.rs:932` defines `git_watch_loop`:

- Spawned as a tokio task at `cmd_daemon.rs:488-490`.
- Runs `git_dirty_files` (`cmd_daemon.rs:1095`, `git status --porcelain -z`) + `git_head` (`cmd_daemon.rs:1079`) every `GIT_POLL_INTERVAL` (2s per constant at `cmd_daemon.rs:39-46`).
- Compares against previous tick's state; only proceeds when either dirty set or HEAD changed.
- Emits `daemon.head.changed` (`:997`) and `daemon.files.changed` (`:1015`) via the router's emitter.

The lifecycle hook `ext.on_files_changed(&dirty_paths)` at `:1019` is a no-op default (`daemon/ext.rs:73`) — meant for the private extension repo to override. It's a valid seam but the OSS repo doesn't wire it to anything.

### Link 2: Watcher → `parse_into_conn(scope=dirty)` — **live**

`cmd_daemon.rs:1023-1034` builds the scope and calls:

```rust
let scope: Option<&[String]> = if dirty_vec.is_empty() { None } else { Some(&dirty_vec) };
crate::cmd_parse::parse_into_conn(&guard, source_dir, lang, scope)
```

`parse_into_conn` at `cmd_parse.rs:461` accepts `scope: Option<&[String]>` and uses it to short-circuit the tree walk (`cmd_parse.rs:476-490` iterates only the scoped paths; `:619-621` applies the same restriction to the deletion candidate set). Emits `daemon.reparse.complete` at `cmd_daemon.rs:1055` with the parsed/deleted counts + changed file list.

Effort to verify latency: nothing to verify — the scope is honored by construction, and the reparse cost is proportional to `|dirty_set|`.

### Link 3: Re-parse → HDC / complex-build pass re-encodes affected nodes — **missing**

The bead's audit-scope hypothesis was that `HdcPass` runs during enrichment but auto-triggers on reparse. Verified: it does not.

Two independent facts:

1. `git_watch_loop` calls `parse_into_conn` and then emits `daemon.reparse.complete`. It does not touch the enrichment pipeline (no call to `enrichment::run_all` or `run_pass` from within the watcher — grep-verified).
2. `HdcEnrichmentPass` (`hdc_enrich.rs`) and `ComplexBuildPass` (`complex_build_pass.rs:221`) are both registered in the enrichment pipeline (`cmd_daemon.rs:292-303`) but the pipeline only executes on `op_enrich` (`daemon/ops.rs:584-634`) — a consumer-invoked wire op.

`HdcEnrichmentPass::run` at `hdc_enrich.rs:119-263` does correctly respect the `changed_files` scope (skips rows whose path is not in the set at `:165-169`), so if the wire were connected the incremental behavior is there.

`ComplexBuildPass::run` at `complex_build_pass.rs:250-290` reads `observation` rows (not the reparse output) — its granularity is per-observation, not per-file. Its dependency on `SessionObservationPass` means the "affected regions" concept it builds is bounded by whichever session-level observations were captured, not by the dirty-file set.

### Link 4: Re-encode → recompute restrictions / `detect_violations` — **missing (as a watcher-driven step)**

The math is reachable but not from the watcher path. Two call sites:

1. `op_agreement` at `daemon/ops.rs:2847`: builds a small `CellComplex` from `payload_inline` rows and calls `detect_violations` synchronously. This is a per-query op — it computes agreement on demand, not as part of any file-change reaction. `DETECT_VIOLATIONS_REACH_COUNT` spy (tests at `ops.rs:5127`, `:5215`, `:5306`) proves the call happens.
2. `HvCellComplex::detect_violations` in `hdc/src/sheaf.rs:323` is not called from any daemon runtime path — only from benches (`hdc/benches/hdc_perf.rs`). Grep-verified: the daemon uses `leyline_sheaf::complex::CellComplex`, not `leyline_hdc::sheaf::HvCellComplex`.

`ComplexBuildPass` invokes `cx.build_delta_0().nnz()` (`complex_build_pass.rs:273`) as a mechanical-reach witness, but the constructed `CellComplex` is dropped when `run()` returns. `SheafState.cache` (which owns the runtime backing complex) is only mutated by consumer-invoked sheaf ops (`sheaf_ops.rs:181` set_topology, `:301` invalidate, `:477` update_topology). Grep verified.

### Link 5: Violation set → `daemon.sheaf.*` invalidation event — **missing**

Full inventory of `sheaf.*` and `daemon.*` emissions (grep `emitter.emit` + literal topics under `rs/ll-open/cli-lib/src/`):

- `cmd_daemon.rs:535` — `daemon.snapshot`
- `cmd_daemon.rs:997` — `daemon.head.changed`
- `cmd_daemon.rs:1015` — `daemon.files.changed`
- `cmd_daemon.rs:1056` — `daemon.reparse.complete`
- `daemon/embed.rs:423` — `daemon.embed.complete`
- `daemon/socket.rs:300` — `daemon.<op>` (post-op fanout, formatted at runtime; `<op>` is one of `STATE_CHANGING_OPS` = `["load","reparse","flush","snapshot","enrich"]` per `daemon/ops.rs:34`)
- `daemon/sheaf_ops.rs:273, 659` — `sheaf.topology`
- `daemon/sheaf_ops.rs:336` — `sheaf.invalidate`

The `sheaf.*` topics are emitted **only** from within `sheaf_ops.rs` handlers, which are invoked by consumer UDS calls. `daemon.reparse.complete` fires after a scoped reparse but does not carry any region-level information — it's a signal that parsing finished, not a cascade of affected regions.

There is no code path from `git_watch_loop → op_sheaf_invalidate` or `enrichment → op_sheaf_invalidate`. The math exists; the wire does not.

### Link 6: Invalidation event → mache cache flush via UDS — **live conditionally**

`mache/internal/leyline/sheaf_subscriber.go` opens a long-lived `subscribe` connection at daemon startup, filters for `sheaf.invalidate`, and dispatches parsed `SheafInvalidateEvent{Invalidated, Generation, Count}` values to a handler. Regression-guarded by `sheaf_subscriber_e2e_test.go`. The pre-v0.4.3 bug (`ley-line-open-5caa59`) that dropped events silently was fixed with per-connection writer + per-subscribe relay tasks.

So: when a `sheaf.invalidate` event **does** fire, mache reliably consumes it. But since link 5 is missing, the event only fires when mache itself (or another consumer) has just called `op_sheaf_invalidate` — i.e. this closes a consumer-owned loop, not a source-change-owned one.

### Link 7: mache → agent fresh answer — **live conditionally**

`SheafClient.InvalidateWithStalk` (`mache/internal/leyline/sheaf.go:146-175`) sends `op_sheaf_invalidate` and returns the daemon's BFS-ordered cascade list. Mache uses that list to evict its own per-region node caches, so subsequent `find_callers` / `find_callees` queries hit the live db instead of stale entries. `sheaf_e2e_test.go` proves end-to-end round-trip.

Same caveat as link 6: this is reachable only when mache (or another consumer) drives the invalidation. Not driven by LLO file-change observations.

## Gaps found

### Gap 1: watcher → enrichment plumbing (link 3)

- **Current state:** `git_watch_loop` runs a scoped `parse_into_conn` and stops. Enrichment (HDC re-encode, complex-build) is not invoked. A consumer must call `op_enrich` to get any of that work done.
- **What's missing:** After a successful reparse, the watcher should either (a) run `enrichment::run_all(passes, conn, source_dir, Some(&dirty))` so the incremental passes re-encode the affected files, or (b) emit a `daemon.reparse.enrich_available` signal that a consumer (mache) can react to by calling `op_enrich` with the reported `changed_files`.
- **Effort:** M. Requires deciding whether the enrichment lock is held long enough to run synchronously in the watcher loop, or whether a background enrichment task is spawned. `TreeSitterPass` is a no-op (it wraps the same `parse_into_conn`), so the real cost is HDC + LSP + complex-build.
- **Recommended follow-up bead scope:** "Wire git_watch_loop to auto-run enrichment on dirty scope; measure per-tick latency budget; add a config knob to opt out for consumers who want to schedule enrichment themselves."

### Gap 2: ComplexBuildPass drops its complex on return (links 3-4)

- **Current state:** `ComplexBuildPass::run` builds a `CellComplex` from `observation` rows, calls `build_delta_0` as a mechanical-reach witness, then drops the complex when the function returns (`complex_build_pass.rs:241-248` — writes set is empty; the module doc says "V1 stores the derived complex + tracker in process memory").
- **What's missing:** The pass should either (a) install the constructed complex into `SheafState.cache` (via a new `SheafCache::set_complex` on the shared arc), or (b) persist it to SQL for downstream reuse. Without one of these, every consumer query pays to rebuild the complex.
- **Effort:** M. Option (a) is a lock-ordering exercise: the pass currently owns nothing after `finalize`; wiring into `SheafState.cache` requires holding the cache mutex across the swap and taking care not to race consumer `sheaf_*` ops mid-flight. Option (b) is a schema migration and read-back on next-consumer-op.
- **Recommended follow-up bead scope:** "Land `SheafCache::install_complex_from_observations` (option a) and call it from ComplexBuildPass. Add a test proving `op_sheaf_invalidate` after `op_enrich pass=complex-build` cascades through the newly-installed complex."

### Gap 3: no watcher-driven `sheaf.invalidate` emit (link 5)

- **Current state:** `daemon.reparse.complete` carries `changed_files` but no region-level information. `sheaf.invalidate` is only emitted from consumer-invoked handlers.
- **What's missing:** After a reparse (and enrichment), the daemon should map `changed_files → touched regions` (using the current SheafCache topology) and emit `sheaf.invalidate` on the touched region set. This is the load-bearing step for the "region-precise invalidation" claim.
- **Effort:** L. Needs (a) a persistent file → region mapping (currently region ids are consumer-defined and consumer-owned — mache computes them from Louvain community detection), or (b) an explicit contract that consumers register their file→region maps with the daemon at topology-push time. Without (a) or (b), the daemon doesn't know which regions a changed file belongs to.
- **Recommended follow-up bead scope:** "Design ADR for consumer-registered file→region maps on `sheaf_set_topology`; implement watcher-driven `op_sheaf_invalidate` self-call using the map; benchmark cascade latency vs the current consumer-owned polling loop."

### Gap 4: `on_files_changed` / `on_head_changed` are no-op stubs (adjacent to link 3)

- **Current state (audit):** `DaemonExt::on_files_changed` (`daemon/ext.rs:73`) and `on_head_changed` (`:67`) are empty defaults. The private repo can override, but OSS LLO has no override.
- **What's missing (audit):** Either (a) delete the hooks (the file-changed event already covers the same signal and can be subscribed to from any extension), or (b) wire the OSS default to call into enrichment / sheaf-invalidate so the OSS repo has the same loop the private repo does.

**Resolution (2026-07-08, post-gap-1)** — Path (c): the hooks stay as extension seams for private-repo-specific side-effects; OSS's invalidation loop is wired DIRECTLY in `git_watch_loop` (gap 1 landed via PR #138) rather than routing through the hook.

Docstring updated 2026-07-08 to make the extension-seam vs OSS-loop distinction explicit. See `rs/ll-open/cli-lib/src/daemon/ext.rs` line 61+ — both hooks now carry a "Relationship to the OSS sheaf-invalidation loop" section clarifying that:

- OSS invalidation is direct — watcher → enrichment → sheaf.invalidate emit
- These hooks fire ALONGSIDE the OSS loop, not through it
- Private-repo extensions may use them for their own side-effects
- Default no-op reflects the correct OSS behavior (OSS has no work to do here)

No code change beyond docstrings. Marketing claim ("moat is closed inside LLO") is now defensible without asterisk on this gap.

## Marketing implication

If the LLO strategic-analysis pitch is "our sheaf-based topology gives
region-precise invalidation, unlike codebase-memory-mcp / Serena /
GrapeRoot", then in its current form that claim is defensible only with
careful phrasing. What's actually shipping:

- **The math is real and tested:** `CellComplex`, `SheafCache`, δ⁰-driven
  cascade, XOR-Merkle pre-filter, co-change tracker, agreement violations,
  reap. All proven with unit + gate tests.
- **The wire is real and tested:** `sheaf_set_topology`, `sheaf_invalidate`,
  `sheaf_update_topology`, `sheaf_reap`, `sheaf_defect`, subscribe/replay,
  mache's `SheafClient` + `SheafSubscriber`. End-to-end tests in mache.
- **The loop is not closed inside LLO:** the daemon does not observe file
  changes and produce region-precise cascades on its own. Consumers push
  topology, poll or push invalidation themselves, and consume the daemon's
  cascade response. The daemon is a math coprocessor for consumer-driven
  cascades.

Recommended positioning until Gaps 1–3 close: "LLO ships a topology-aware
cache-coherence substrate that mache (or any other consumer) can drive to
get region-precise cascades in O(BFS depth) rather than blast-radius
whole-repo evictions. The consumer owns the topology and change signal;
LLO owns the algebra."

Before claiming "sheaves as the moat" without qualification, close Gap 3
(watcher-driven emit) at minimum — that's the one that makes the loop
closed inside the daemon.

## Cross-references

- Bead this audit fulfills: `ley-line-open-a3f764`
- Sheaf primitives crate: `rs/ll-open/sheaf/src/lib.rs`, `cache.rs`, `complex.rs`, `topology.rs`
- Wire ops: `rs/ll-open/cli-lib/src/daemon/sheaf_ops.rs`
- Watcher: `rs/ll-open/cli-lib/src/cmd_daemon.rs:922-1092`
- Enrichment pipeline: `rs/ll-open/cli-lib/src/daemon/enrichment.rs`
- ComplexBuildPass: `rs/ll-open/cli-lib/src/daemon/complex_build_pass.rs`
- HdcEnrichmentPass: `rs/ll-open/cli-lib/src/daemon/hdc_enrich.rs`
- Extension seams: `rs/ll-open/cli-lib/src/daemon/ext.rs`
- mache consumer: `~/remotes/art/mache/internal/leyline/sheaf.go`, `sheaf_subscriber.go`
- Related ADRs: ADR-0020 (observation schema + agreement math gates), ADR-0010 (event bus)
- Related closed beads referenced by code:
  `ley-line-open-96b1a9` (HDC + sheaf integration),
  `ley-line-open-c7eae2` (ComplexBuildPass),
  `ley-line-open-5caa59` (sheaf.invalidate delivery fix, LLO v0.4.3),
  `ley-line-open-1a0a2a` (idle-CPU snapshot skip)
