# ADR-0015 — Lazy-on-access ingestion (LSP semantics over the sheaf substrate)

**Status:** Proposed (2026-05-16)
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate)
**Thread:** T9/lazy-on-access-ingestion
**Bead:** `ley-line-open-9db858`
**Pairs with:** ADR-0016 (`ley-line-open-9f491f`) — AI-native query surface. ADR-0015 settles **when** parsing happens; ADR-0016 settles **what shape** consumers see. The two are deliberately split so each can be adopted independently.

---

## Context

Today's daemon is eager. `leyline daemon` against the mache repo (764 files at the snapshot used for PR #16's benches) pays ~5.3s of cold tree-sitter parsing before the first op can answer, and the resulting on-disk cache settles near 3.5 GB (`_ast` rows dominate). Every file is parsed once, every binding is materialised, every reference edge is written — whether or not a consumer ever asks. The eager pass has no skip-on-stalk-match: even when the sheaf cache (PR #16, commit `7a919e1`) could prove that a file's stalk hash is unchanged from a prior generation, the parser still runs because parsing is the only producer.

Three observed problems flow from that shape:

1. **Cold start dominates wall-clock latency** for short-lived consumers. A mache `find_callers` call against a freshly-started daemon waits for all 764 files, not just the file(s) on the answer path.
2. **Cache mass is decoupled from access mass.** The majority of `_ast` rows in the mache snapshot are never read by any subsequent op in a typical agent session — the eager pass treats every file as equally likely to be queried, when in practice access is sparse. The exact ratio is a motivating observation, not a measurement gate of this ADR; §7's falsifiability gives a measurable counterpart (cold daemon has zero `_ast` rows until accessed).
3. **The sheaf substrate is a freshness oracle, but no consumer pays attention to it.** PR #16 wired `SheafCache` + `CoChangeTracker` into the daemon (`rs/ll-open/cli-lib/src/daemon/sheaf_ops.rs`), giving us per-stalk freshness signals. Eager ingestion never reads those signals — it just re-parses.

The lazy-on-access pivot inverts the data flow: **ingest light** (a path-and-metadata index, plus the topology-pre-pass output), **parse on access** (when a consumer op needs AST/binding data for a file), **cache through the sheaf** (entries keyed by stalk hash, evicted by the existing restriction-graph invalidation). The SQL tables become a cache of materialised answers, not the source of truth; the source of truth is the file tree plus the capnp segment files emitted by parses that actually happened.

LSP has been doing roughly this since 2016 (didOpen tells the server *which* files the user cares about; the server parses on demand). LLO is not implementing LSP — ADR-0016 settles the consumer-facing surface — but LSP is the existence proof that lazy-on-access scales to multi-language repos and outperforms batch indexers for interactive consumers.

This ADR commits to seven sub-decisions. Each is structured: alternatives considered ≥ 2, choice, falsifiability criterion (a concrete observable that fails CI if the rule is violated).

---

## Decision

### 1. Trigger semantics — which syscalls force a parse?

**Alternatives considered.**

- (a) **Parse-on-`open(2)`.** Any consumer that opens a file fd would trigger a parse. Simplest to wire; matches the "open = intent" intuition. Rejected: editors and indexers routinely `open` files for trivial metadata reasons (mache's directory walker `open`s every file to read a 4 KB header for language sniffing), so this would re-create the eager-ingest cost wearing a different hat.
- (b) **Parse-on-`stat(2)`/`getattrlist(2)`.** Even more aggressive than (a); makes `find` over a directory parse everything. Rejected on the same grounds, more strongly.
- (c) **Parse on content access only** — `read(2)`, `pread(2)`, `readv(2)`, `mmap(2)` page-fault, `sendfile(2)`. Metadata syscalls (`stat`/`lstat`/`fstat`/`statx`/`getattrlist`/`access`/`faccessat`/`readdir`/`getdents`) are answered from the light index without touching the parser. **Chosen.**

Choice rationale: POSIX cleanly partitions syscalls into "metadata returns to userspace" (`<sys/stat.h>` family; IEEE Std 1003.1-2017) and "file content returns to userspace" (`<unistd.h>` read family; `<sys/mman.h>` for `mmap`). macOS's `getattrlist(2)` is a metadata-only BSD extension — it never reads file content (Apple `getattrlist(2)` manual page; `<sys/attr.h>` enumerates the attribute groups, none of which include file content). The partition is stable across both platforms, so the trigger rule is portable.

**Falsifiability.** A benchmark that walks a 10K-path tree calling only `stat(2)` (or, on macOS, `getattrlist(2)` for batch metadata) MUST produce zero tree-sitter parser invocations. Pass: parser invocation counter (instrumented in `cmd_parse.rs`) reads zero after the walk. Fail: any non-zero parse count.

---

### 2. Light-index shape — what's in the cheap-on-ingest tier?

**Alternatives considered.**

- (a) **Filename + size only.** Cheapest possible. Rejected: lacks the regex-derived import edges that the topology pre-pass produces, so `find_callers` on a cold cache would have no neighbour set to bound its 1-hop expansion (see §4).
- (b) **Filename + size + content hash.** Adds hashing cost on ingest. Rejected as default: hashing 3.5 GB of source at ingest defeats the "light" objective; the sheaf substrate already content-addresses on parse, and a stat-derived `(size, mtime, ino)` triple is sufficient to detect the *did-this-change?* question that ingest needs to answer.
- (c) **Paths + sizes + mtimes + manifest contents + regex-derived import edges** (output of the topology pre-pass — bead `ley-line-open-3a7c12`'s artifact). Manifests (`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`) are parsed eagerly because they're cheap (KB-scale, few of them, language-specific simple parsers) and they constrain symbol resolution. **Chosen.**

Choice rationale: this is exactly the set the topology pre-pass already produces. Reusing its output avoids a second pass over the file tree on daemon start. The `(size, mtime)` pair is sufficient to drive cache-validity checks because the sheaf already gates on stalk hash for any cell whose stalk has actually been computed — the light index only needs to answer "is this entry potentially stale?", not "what is its current hash?".

**Falsifiability.** For the mache repo at the PR #16 snapshot (~764 files), the light index MUST:
- Fit in < 2 MB on disk (single capnp segment file or sqlite blob).
- Build cold in < 200 ms wall-clock on the bench host (Apple M-series; matches PR #16's bench host).

Pass: `ls -l <index-file>` < 2 097 152 bytes AND `time leyline daemon --light-only` reports startup time < 200 ms. Fail: either bound exceeded.

---

### 3. Cache backing — where do parsed-on-miss results live?

**Alternatives considered.**

- (a) **A fresh LRU keyed by `(path, mtime)`.** Rejected: duplicates work the sheaf already does. Keying by mtime is also wrong on platforms where mtime can be backdated (`utimensat(2)`, git checkout's preservation of source mtimes).
- (b) **SQLite-only — extend `_ast` to be a write-through cache.** Rejected: SQL tables are projections per ADR-0014 §Context; making them authoritative again re-creates the be6136 class (schema-as-protocol). Cache identity must live on the substrate, not on the projection.
- (c) **Extend `SheafCache` from PR #16.** Entries keyed by stalk hash; invalidation already implemented via `SheafCache::on_change` walking the restriction graph (`rs/ll-open/sheaf/src/cache.rs`). New cache layer is parsed-AST blobs hung off the existing stalk-hash key. **Chosen.**

Choice rationale: the sheaf cache already enforces the invalidation invariant we need ("any change to a stalk evicts every cache entry reachable via the restriction graph from that stalk"). Reusing it means we get the invalidation correctness story for free; we add a value type, not a new index.

**Falsifiability.** Test fixture: build a 3-file restriction graph A → B → C (A imports B, B imports C); parse all three; flip C's stalk hash (deliberate edit). On the next `SheafCache::on_change(C)` call, the cache entries for A, B, and C MUST all be evicted (the invalidation is graph-reachable, not just direct). Pass: `falsifiability_gates.rs` test "Claim 2: `SheafCache::on_change` invalidates only restriction-graph-reachable entries" (already present at `rs/ll-open/sheaf/tests/falsifiability_gates.rs:150`) is extended to assert eviction reaches A, B, C from a C-change. Fail: any of A, B, C remains in cache after the change.

---

### 4. Miss policy — what gets parsed on miss?

**Alternatives considered.**

- (a) **Parse the single file only.** Rejected: symbol resolution requires the importing file's bindings to be known. A `find_callers("foo")` answer in file X depends on having parsed every file that imports X (or at least every file whose import-edge set names X's symbols). Parsing only X gives wrong-but-confident answers.
- (b) **Parse the full transitive import closure of the file.** Rejected as default: unbounded; worst case re-parses the whole repo on a single miss, undoing the laziness premise.
- (c) **Parse the single file plus its 1-hop import neighbours** (incoming + outgoing edges from the topology pre-pass's import-edge index). Bounded by the in-degree + out-degree of the file in the import graph. **Chosen.**

Choice rationale: 1-hop is the smallest bound that's correct for symbol resolution against the language semantics that resolve direct imports without traversing re-exports — Rust's `use` paths and Python's `from x import y` are the cleanest cases. The bound is *not* sound by itself for TypeScript barrel files (`export * from "./other"`) or Go anonymous-field method promotion across packages; for those languages the implementation MUST treat re-export edges as part of the 1-hop set (the topology pre-pass produces them as a distinct edge kind). The bound stays "1 hop in the topology graph"; the topology graph itself encodes the language-specific transitive cases.

**Falsifiability.** On a cold cache, `op_find_callers(token = "foo")` against a file X with N 1-hop neighbours MUST parse strictly fewer than `N + 1 + 1` files (X + N neighbours; the strictly-less-than catches double-parses of any neighbour). Instrument: parser-invocation counter incremented per file in `cmd_parse.rs::parse_file`; assert via test against a fixture repo with known import topology. Pass: counter < `N + 2`. Fail: counter ≥ `N + 2`.

---

### 5. Change detection — fsnotify mechanism?

**Alternatives considered.**

- (a) **Polling — periodic `readdir` + `stat` sweep.** Rejected: doesn't scale (O(files) per tick) and produces unbounded notification latency.
- (b) **`kqueue(2)` per file on macOS / `inotify(7)` per file on Linux, watching every source file.** Rejected: `kqueue` `EVFILT_VNODE` requires an open file descriptor per watched file (FreeBSD `kqueue(2)` man page, "EVFILT_VNODE" section; Apple `kqueue(2)` page inherits the same semantics). 10K+ source files would consume 10K+ fds on macOS where the default soft limit is 256 (`launchctl limit maxfiles`) and the hard ceiling is OS-version-dependent. `inotify` has a per-user watch limit (`/proc/sys/fs/inotify/max_user_watches`, default 8192 on most distros pre-2023, 1 048 576 on systemd 250+). Per-file is a footgun.
- (c) **FSEvents on macOS at directory-tree granularity; `inotify` at directory granularity on Linux; `kqueue` per-file only for the subset of files with an active sheaf-cache entry.** **Chosen.**

Choice rationale: FSEvents was designed for the directory-tree-scale use case (Apple File System Events Programming Guide: "FSEvents is appropriate when you want to be notified about changes to the contents of a directory hierarchy" — note: per Apple's own docs, events are coalesced and the `kFSEventStreamEventFlagMustScanSubDirs` flag indicates that events were dropped under sustained churn; consumers MUST handle that flag by re-scanning). `inotify` at directory granularity sidesteps the per-watch limit. `kqueue` reappears only for the small set of files whose parsed AST is hot in the sheaf cache — those need per-file precision so that a single-file edit can invalidate the right cache entry without a directory rescan.

**Falsifiability.** A test that edits any source file in the watched tree MUST produce a cache-invalidation event at the daemon within 500 ms wall-clock (the gate against polling fallback). On macOS, the test additionally MUST exercise the `kFSEventStreamEventFlagMustScanSubDirs` path — either by stress-driven churn that forces FSEvents to coalesce/drop, or by synthetic injection of a `MustScanSubDirs` event in the stream consumer — and verify the daemon responds with a rescan rather than silently missing affected paths. Pass: `t_edit_to_invalidate_ms` < 500 AND `must_scan_subdirs_handled == true` in the test log. Fail: either condition.

---

### 6. Consumer contract — what changes for mache / agents?

**Scope.** This decision is restricted to the lazy-access concerns ADR-0015 owns: how consumers detect staleness when answers may come from cache versus a fresh parse. **The broader question of what shape consumers should see — symbol-keyed vs position-keyed lookup, bundled vs round-trip responses, structured vs markdown payloads, LSP compatibility — is settled in the sibling ADR-0016 (`ley-line-open-9f491f`), not here.** ADR-0015 stays surgical: one additive field; existing fixtures stay green.

**Alternatives considered.**

- (a) **Break existing responses to add staleness inline as a wrapping envelope.** Rejected: violates ADR-0014's append-only-additive evolution rule and breaks the cross-runtime drift gate (`daemon-protocol.json`'s `response_required_keys` would shift, causing mache's Go strict-unmarshal test to fail).
- (b) **Out-of-band staleness via a separate `freshness` op.** Rejected: two-round-trip cost on every consumer call, and there's a TOCTOU window between the answer call and the freshness call where a file change could land between them.
- (c) **In-band optional `freshness` field, additive only.** Every consumer op response MAY include a `freshness: { generation, parsed_at_ms, source_mtime_ms }` object. The field is added to each op's `response_optional_keys` in the `daemon-protocol.json` fixture; the field is never required, so old consumers continue to deserialize successfully. **Chosen.**

Choice rationale: this matches the existing precedent in the fixture — `status`'s `response_optional_keys: ["head_sha", "error", "enrichment"]` already documents the same pattern for optional fields. Adding `freshness` is an instance of that pattern, not a new convention. The capnp-json wire (per ADR-0014 §3) emits Int64 / UInt64 as JSON strings, so `generation`, `parsed_at_ms`, and `source_mtime_ms` are quoted in the worked example.

**Worked example.** Today's `find_callers` fixture (`rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` lines 107-119):

```json
"find_callers": {
  "request": {"op": "find_callers", "token": "some_fn"},
  "response": {
    "ok": true,
    "callers": [
      {"node_id": "file1.rs#fn1", "source_id": "file1.rs"}
    ]
  },
  "response_required_keys": ["ok", "callers"],
  "response_optional_keys": [],
  "go_binding": "FindCallersResponse",
  "go_drift_skip": null
}
```

Post-ADR-0015 shape (the only diffs are `freshness` inside `response` and the new entry in `response_optional_keys`; `response_required_keys` is unchanged):

```json
"find_callers": {
  "request": {"op": "find_callers", "token": "some_fn"},
  "response": {
    "ok": true,
    "callers": [
      {"node_id": "file1.rs#fn1", "source_id": "file1.rs"}
    ],
    "freshness": {
      "generation": "42",
      "parsed_at_ms": "1715000000000",
      "source_mtime_ms": "1715000000000"
    }
  },
  "response_required_keys": ["ok", "callers"],
  "response_optional_keys": ["freshness"],
  "go_binding": "FindCallersResponse",
  "go_drift_skip": null
}
```

The Go strict-unmarshal test in `clients/go/leyline-schema/daemon/` continues to pass because (1) `freshness` is absent from `response_required_keys`, so the existing required-key check is unchanged, and (2) the Go binding `FindCallersResponse` gains an optional `Freshness *FreshnessInfo \`json:"freshness,omitempty"\`` field; capnp's canonical encoding (per ADR-0014 §1) guarantees that an instance that doesn't set `freshness` produces the same bytes as before. The Rust handler-output test gains a counterpart assertion that, when staleness is irrelevant (e.g., op called against the cold light index), the field MAY be omitted entirely.

**Forward to ADR-0016.** The shape of `freshness` itself may be refined in ADR-0016 (it already plans a `freshness: { generation, parsed_at_ms, source_mtime_ms, stalk_hash }` quadruple; the additional `stalk_hash` field is also additive and would not break ADR-0015's gate). The decision of *what other fields* responses should carry — `hover_typed`, `references`, `implementations`, bundled neighbourhoods — is ADR-0016's, not ADR-0015's.

**Falsifiability.** The existing cross-runtime drift gate (`rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` consumed by both the Rust handler-output test and the Go strict-unmarshal test, per ADR-0014 §Implementation status) MUST remain green after `freshness` lands in `response_optional_keys` for every op. Pass: `task ci` green across both Rust and Go test runs with `freshness` added to all 17 typed-surface op fixtures. Fail: any Rust or Go test failure attributable to the field addition.

---

### 7. Eager fallback — when does eager ingest still run?

**Alternatives considered.**

- (a) **Remove eager ingestion entirely.** Rejected: CI indexing jobs (mache's bulk indexer; the cloister build pipeline) want a single command that produces a complete DB. Forcing them to fake an op-call traversal would be silly.
- (b) **Eager by default; opt-in lazy.** Rejected as the inverse of the desired pivot — short-lived consumers and editor-style users (the majority of agent traffic) would still pay the cold-start tax.
- (c) **`leyline parse <dir>` CLI command stays eager** (one-shot bulk load — the historical behaviour); **`leyline daemon` runs lazy** (the new shape). Both produce the same DB schema; the difference is *when* the rows are written. CI / batch consumers keep their existing entrypoint; interactive consumers get the lazy path. **Chosen.**

Choice rationale: the two consumer profiles have genuinely different needs. Batch jobs want predictable wall-clock for a known input set; interactive jobs want low latency on the access path and don't care about completeness of cold rows. Splitting on entrypoint (not on a daemon flag) is the cleanest separation — no ambient mode that a misconfigured launcher could flip.

**Falsifiability.** `leyline parse <dir>` against a fixture repo MUST produce a DB byte-equivalent to today's (modulo timestamps; rows compared by content hash). `leyline daemon` started cold against the same fixture MUST report zero rows in `_ast` until the first op-call that requires a parse. Pass: `sqlite3 <db> "SELECT COUNT(*) FROM _ast"` returns `0` immediately after daemon startup AND equals the eager value after a `find_callers` call against every source file. Fail: nonzero `_ast` count at daemon startup, or `parse`-vs-`daemon` DB content divergence.

---

## Consequences

### Positive

- **Cold-start latency collapses** for short-lived consumers. The light index builds in < 200 ms (§2); the first op call parses only its access-path files (§4); the cold-start tax is paid where the work actually lives.
- **Cache mass tracks access mass.** Files never accessed by any op never enter `_ast`. The 95 % unused-row figure becomes ~0 % for interactive consumers; CI consumers keep eager via `leyline parse` (§7).
- **Sheaf substrate becomes load-bearing.** PR #16 wired the cache; lazy access is what makes that cache useful. `SheafCache::on_change` invalidation (§3) is the correctness boundary for the entire architecture.
- **Consumer compatibility is structurally preserved.** Decision 6 keeps `response_required_keys` invariant; existing mache builds continue to deserialize daemon responses unchanged.
- **Editor / IDE precedent.** LSP servers have been doing this since 2016. The architectural pattern is battle-tested by `gopls`, `rust-analyzer`, `clangd`; we are not pioneering, we are catching up.

### Negative

- **Worst-case latency on cold miss is worse than today's worst case.** A `find_callers` against a file with a large 1-hop neighbour set parses the file plus its neighbours inside the consumer's request budget; today's eager pre-pay shifts that cost to startup. Mitigation: the light index covers most metadata ops without parsing; the cache amortises hot symbols across consumer calls; pathological topology-graph hubs (e.g., utility crates imported by hundreds of files) become a known latency hot-spot to monitor rather than a hidden one.
- **Two ingestion code paths.** Eager (`leyline parse`) and lazy (`leyline daemon`) must produce DB-shape-equivalent outputs (§7's falsifiability). Drift between the two is a real risk; the cross-runtime fixture suite from ADR-0014 §3 is the structural defence.
- **fsnotify is platform-specific.** FSEvents (macOS), `inotify` (Linux), and the to-be-decided Windows mechanism (`ReadDirectoryChangesW`, deferred) each have their own coalescing / drop semantics. Per-platform integration tests are required, not optional. The `MustScanSubDirs` flag handling (§5) is the macOS-specific correctness lifeline.
- **`leyline daemon` against a cold daemon will under-report total counts.** Ops like `get_refs_map` that today return a complete map become bounded by what's been parsed so far. ADR-0016 will decide whether to expose this as a `freshness.coverage` field or to force-parse on those ops; ADR-0015 does not pre-decide.

### Out of scope (future ADRs)

- **Cache eviction policy beyond sheaf-driven.** PR #16 settles invalidation; size-bounded eviction (LRU, ARC, etc.) for the parsed-AST blob layer is a separate question once cache mass becomes a problem.
- **Windows fsnotify.** `ReadDirectoryChangesW` has its own ordering / buffer-overflow semantics. Deferred until a Windows consumer is real.
- **The AI-shaped consumer surface.** Bundled responses, symbol-keyed lookup, structured hover, stateless ceremony — all ADR-0016.
- **Implementation crate split** (mentioned as a non-goal in the bead). The bead explicitly says "don't pre-decide" and this ADR honours that.

---

## References

- **Sheaf PR #16** — `commit 7a919e1`, `feat(sheaf): lift leyline-sheaf into ll-open + daemon UDS/MCP wiring`. Introduced `SheafCache` + `CoChangeTracker` and the daemon wiring this ADR builds on.
- **`rs/ll-open/sheaf/tests/falsifiability_gates.rs`** — already-shipping invalidation invariants; §3's falsifiability extends the existing Claim 2 test.
- **`rs/ll-open/cli-lib/src/daemon/sheaf_ops.rs`** — daemon-side sheaf cache wiring.
- **`rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json`** — cross-runtime drift gate; §6's worked example pins the additive shape.
- **ADR-0014** — Cap'n Proto as the producer/consumer protocol; canonical encoding + append-only evolution rules underpin §6's compatibility argument.
- **ADR-0016** (`ley-line-open-9f491f`) — AI-native query surface; settles consumer-shape questions ADR-0015 explicitly forwards.
- **Topology pre-pass bead** — provides the regex-derived import-edge index that §2's light index and §4's 1-hop expansion both consume.
- **IEEE Std 1003.1-2017** (POSIX.1-2017) — `<sys/stat.h>` (metadata-only syscalls), `<unistd.h>` (read family), `<dirent.h>` (`readdir`), `<sys/mman.h>` (`mmap`). Authoritative reference for §1's metadata-vs-content partition. <https://pubs.opengroup.org/onlinepubs/9699919799/>
- **Apple `getattrlist(2)` manual page** — confirms metadata-only semantics. <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getattrlist.2.html>
- **Apple File System Events Programming Guide** — FSEvents coalescing + `kFSEventStreamEventFlagMustScanSubDirs` semantics. <https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/>
- **FreeBSD / Darwin `kqueue(2)` man page** — `EVFILT_VNODE` per-fd requirement. <https://man.freebsd.org/cgi/man.cgi?query=kqueue>
- **Linux `inotify(7)` man page** — per-watch-descriptor model, `max_user_watches` sysctl. <https://man7.org/linux/man-pages/man7/inotify.7.html>
- **LSP specification (3.17)** — existence proof for lazy-on-access at multi-language repo scale. <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>
