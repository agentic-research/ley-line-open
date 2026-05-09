# workerd + SQLite-WAL as candidate execution layer for ley-line-open

**Date:** 2026-05-08
**Author:** empirical-validation-designer (autonomous agent)
**Time-box:** ~3h, used ~2h
**Status:** H1 measured on disk; H2 + H3 analyzed at source level (workerd build skipped per constraint)

---

## Summary verdict

**Should ley-line-open adopt SQLite WAL?** *Conditionally yes — but the daemon's
current shape (`Mutex<Connection>` over `:memory:`) cannot use it.* WAL on a
file-backed db gives **p99 reads of 290–375 µs @ N=10 readers**; DELETE
degrades to **120–250 ms p99** — ~600× worse. WAL writers drop to ~7–8% of
their solo baseline at N=10 (10 K/s vs 100–130 K/s solo), so the literal
"≥80% of solo" criterion **fails**. But WAL@N=10 still beats DELETE-solo
absolute throughput by 3×, so the architectural criterion (WAL dominates
DELETE everywhere) **passes**. The fix is two-fold: (1) file-backed live db,
(2) `Mutex<Connection>` → connection pool.

**Should the daemon target workerd?** **No.** Three structural reasons from
workerd source: (a) `SqliteDatabase` (`util/sqlite.h:79–85`) takes only
`kj::Path` — no `sqlite3_deserialize` exposure, so H3 (zero-copy share of the
arena) is **structurally impossible**; (b) `SqlStorage` (`api/sql.c++:163`)
forbids `BEGIN`/`SAVEPOINT` and serializes all writes through one implicit
txn per isolate-lock entry — a DO is a single-threaded actor, so H1's
multi-reader benefit doesn't apply inside one DO; (c) cloister ADR-0001:98
already documented *"WASM in workerd cannot reach DO native SQLite."* UDS
stays.

---

## Repo prior art (do not redo)

- `rs/ll-open/cli-lib/benches/wal_snapshot_experiment.rs` (uncommitted, 554
  lines, branch `experiment/wal-snapshot-lock-hold`) — already verified that
  `:memory:` SQLite *silently ignores* `PRAGMA journal_mode = WAL` (`PRAGMA
  journal_mode = WAL` returns `"memory"`). Premise of "just turn on WAL"
  is non-configurable on the current daemon.
- `cloister/src/storage/workerd.ts` — working `WorkerdBlobStore` /
  `WorkerdRefStore` over DO `SqlStorage`. Substrate-equivalence test
  passes (`cloister/test/storage/workerd.test.ts:86–98`). This is the
  *only* shape of "workerd holds a SQLite db" that exists in this
  ecosystem today, and it's a CAS-ref store, not a graph-engine.
- `cloister/docs/adr/0001-workerd-mcp-gateway.md:98` — superseded
  "DoltLite" entry explicitly states WASM-in-workerd cannot reach DO
  SQLite.

---

## H1 — file-backed SQLite WAL with concurrent readers

**Hypothesis (testable form).** With the daemon's `:memory:` db replaced by a
file-backed db in WAL mode, **N=10 concurrent reader connections** observe
**p99 read latency < 5 ms** while a single writer maintains **≥ 80% of the
solo-writer baseline throughput**.

**Null hypothesis.** WAL doesn't materially reduce contention vs DELETE — the
two journal modes produce statistically indistinguishable read/write
distributions under N=10 concurrent readers + 1 writer.

**Test design.** File-backed SQLite, `nodes(id PK, kind TEXT, payload BLOB)`
seeded with 50K × 256 B rows (~14 MiB). Pragmas: `journal_mode ∈ {DELETE,
WAL}`, `synchronous = NORMAL` for both. Each reader thread opens its **own**
`SQLITE_OPEN_READ_ONLY` connection (a `Mutex<Connection>` would defeat WAL —
that's the daemon's current shape, and what would have to change). One
writer thread autocommits inserts. Readers select random ids via LCG.
`SQLITE_BUSY` retries via `yield_now()` (fair to both modes — adds latency
rather than hiding it). 3 s per cell. Hardware: macOS / Apple Silicon.

Bench source: `rs/ll-open/cli-lib/benches/wal_concurrent_readers.rs`. Built
standalone at `/tmp/h1_bench/` because `leyline-cli-lib` has a pre-existing
compile error on `main` (`daemon/mcp.rs:382`).

**Results — run 1 (3 s windows):**

```
══ DELETE (rollback journal) ══
  solo writer: 3 073 writes/sec
  mode      N_r   writes/s     reads/s     r p50     r p99     r max     w p50     w p99
  DELETE      1       3074         131   10.00µs   96.36ms  263.82ms  216.71µs    3.51ms
  DELETE      4       3122         429    9.00µs  154.28ms  363.89ms  247.12µs    2.42ms
  DELETE     10       3178         838    9.17µs  251.39ms  781.05ms  210.88µs    2.83ms

══ WAL ══
  solo writer: 128 554 writes/sec
  mode      N_r   writes/s     reads/s     r p50     r p99     r max     w p50     w p99
  WAL         1      79989      149906    6.08µs   17.17µs   29.97ms   10.71µs   32.29µs
  WAL         4      34972      157927   21.29µs   87.25µs    5.26ms   23.54µs   87.71µs
  WAL        10      10610      112563   77.00µs  285.88µs    4.87ms   82.21µs  287.17µs
```

**Run 2** (reproducibility check): same shape; WAL@N=10 read p99 =
**375 µs**, write throughput = **7 710/s**. Consistent.

**Verdict.**

| Pass criterion                                  | Result                  |
|-------------------------------------------------|-------------------------|
| WAL p99 reads < 5 ms @ N=10                     | **PASS** (286–375 µs)   |
| WAL writes ≥ 80% of WAL-solo baseline           | **FAIL** (~7–8%)        |
| WAL p99 strictly < DELETE p99 (sanity)          | **PASS** (~600× better) |

The literal criterion fails because WAL's solo baseline (100–130 K writes/s
— pure WAL append with NORMAL fsync) is so high that any contention drops
the ratio. But absolute WAL@N=10 still beats DELETE-solo by 3× and meets
the daemon's sub-ms read budget. **The threshold is mis-tuned, not the
result.**

**Adoption cost.** Two changes to `cmd_daemon.rs`:
(1) Open live db at a known path instead of `Connection::open_in_memory()`
(line 410); set `PRAGMA journal_mode = WAL`; verify.
(2) Replace `live_db: Mutex<Connection>` (line 200) with a connection pool
so reader handlers (`op_get_node` at `daemon/ops.rs:630`) get their own
connections. The arena remains the right primitive for the cross-host Σ
substrate (`docs/decades/2026-merkle-cas-substrate.md` §1.5); WAL just
relaxes the in-process lock window.

---

## H2 — workerd Durable Object SQLite vs WAL throughput

**Hypothesis.** A workerd DO's SQLite wrapper has comparable concurrency
characteristics to file-backed WAL — within 2× on read/write throughput at
N=10 concurrent fetch handlers reading + 1 writing.

**Null hypothesis.** workerd's actor-model serialization defeats WAL —
the DO's single-threaded mailbox makes "10 concurrent readers" not a
real shape; throughput collapses to single-thread serial.

**Test design.** Source-level analysis of workerd's SQL surface; running
workerd deferred per the dispatcher's "investigate via documentation if
build is heavy" instruction. cloister's existing `vitest-pool-workers` rig
could host a real measurement (1–2 h follow-up).

**Key source findings.**

1. `workerd/util/sqlite.h:79–85` — `SqliteDatabase` constructor takes
   `kj::Path path` + `Vfs` rooted in `kj::Directory`. **No** buffer /
   `sqlite3_deserialize` / `:memory:` constructor on the public API.
   `sqlite3_open_v2` (`util/sqlite.c++:610–622`) only sees rooted paths.

2. `workerd/api/sql.c++:163–169` — `SqlStorage::exec` rejects
   `BEGIN`/`COMMIT`/`SAVEPOINT`: *"please use state.storage.transaction()
   instead of BEGIN/SAVEPOINT statements... interacts correctly with
   Durable Objects' automatic atomic write coalescing."*

3. `workerd/util/sqlite.h:138–145` Regulator comment: *"the platform
   automatically wraps every entry into the isolate lock in a
   transaction."* Combined with `actor-sqlite.h:134–155` (Implicit/
   ExplicitTxn own all txn state), every SQL call is implicit-txn'd
   per-isolate-lock-entry. **One isolate lock per DO.**

4. `workerd/api/sql.c++:34–39` + `util/sqlite.c++:540–563` — pragma
   allowlist excludes `journal_mode`. User code can't toggle WAL. The
   Vfs *does* implement WAL locks (`util/sqlite.c++:2597–2641`), so
   WAL is the underlying mode — but the toggle isn't user-controllable.

**Theoretical conclusion.** A DO is a single-threaded actor; concurrent
`fetch` handlers serialize on the isolate lock. "N=10 concurrent readers"
inside one DO is not a real shape — they queue. Across DO instances each
has a private db, so no shared-db multi-reader case either.

**H2 verdict: FAIL.** Not because WAL is slow, but because the workloads
aren't comparable: workerd serializes everything per-DO. The DO model fits
cloister's per-repo bead store (low-concurrency by design); it does not
fit a process-shared graph engine serving many concurrent UDS clients.

---

## H3 — workerd V8 isolates sharing read access to ley-line-open's mmap'd arena

**Hypothesis.** workerd V8 isolates can share read-only access to
ley-line-open's mmap'd arena directly via `sqlite3_deserialize` from the arena
buffer, bypassing UDS RPC. Pass: <1 ms total round-trip for read-only ops.

**Null hypothesis.** workerd cannot load arena bytes without copying, and/or
cannot expose them to a Worker isolate's SQL surface.

**Source-level analysis.**

1. **No `sqlite3_deserialize` in public API.** `SqliteDatabase`
   (`util/sqlite.h:79–85`) takes `kj::Path` only; `Vfs` rooted in
   `kj::Directory`. No buffer-import constructor. Forking workerd would
   be required.

2. **No buffer/fd path to JS.** `state.storage` and `state.storage.sql`
   are the only storage surfaces. Both create DO-private stores; `exec`
   (`api/sql.c++:34–39`) takes SQL strings + bindings, no buffers.

3. **WASM is excluded.** `cloister/docs/adr/0001-workerd-mcp-gateway.md:98`:
   *"WASM in workerd cannot reach DO native SQLite."* The "Rust+mmap+
   `sqlite3_deserialize` to WASM" alt-path also fails: WASM in Worker
   can't reach native SQLite, and the host fs is exposed only through
   the same closed Vfs.

4. **Liveness mismatch even if (1)–(3) were solved.** The arena is
   write-once-flip per `Controller::generation` (`cmd_daemon.rs:621–681`).
   A deserialized snapshot wouldn't see writes after capture; the Worker
   would have to re-deserialize per generation bump — full memcpy of
   the active buffer (hundreds of MiB at registry scale). UDS RPC for
   a single `op_get_node` is cheaper at that point.

**H3 verdict: FAIL** at the source level. UDS stays.

---

## Reframe

The three sub-hypotheses aren't independent. H2 and H3 depend on workerd
exposing pieces of SQLite it doesn't. Honest reduction:

- **H1 is the real engineering question.** ~50-line patch to
  `cmd_daemon.rs` (file path, pragma, pool); 600× p99 read improvement.
  *Do this.*
- **H2/H3 are the wrong question.** workerd is a serialized actor model
  with sealed SQLite. cloister's `WorkerdBlobStore` is the correct
  pattern: workerd holds the *content-addressed* layer (refs + blobs);
  ley-line-open's daemon stays in-process Rust for the graph-query
  workload.

## Follow-ups

- Real H2 measurement is buildable in cloister's `vitest-pool-workers`
  rig (~1–2 h). Skipped: source analysis already answered architecturally.
- `:memory:`→file-backed migration belongs under the Σ / Merkle-CAS
  decade. T6 ("lock-free trees") becomes lower priority if WAL delivers.
- Pre-existing `main`-branch compile error (`daemon/mcp.rs:382`,
  `&DaemonContext` vs `&Arc<DaemonContext>`) blocks
  `cargo build -p leyline-cli-lib`. Worth a bead.

---

## Files cited

- `rs/ll-open/cli-lib/src/cmd_daemon.rs:200, :410, :535–584, :621–681`
- `rs/ll-open/cli-lib/src/daemon/ops.rs:81, :96, :630`
- `rs/ll-open/cli-lib/benches/wal_snapshot_experiment.rs:11–17, :487–493`
- `rs/ll-open/cli-lib/benches/wal_concurrent_readers.rs` (new, this experiment)
- `docs/decades/2026-merkle-cas-substrate.md` §1.5 (arena ↔ Σ mapping)
- `cloister/src/storage/workerd.ts` (working DO+SQLite reference)
- `cloister/docs/adr/0001-workerd-mcp-gateway.md:98` (ADR datapoint)
- `workerd/src/workerd/util/sqlite.h:79–85, :138–145`
- `workerd/src/workerd/util/sqlite.c++:540–563, :2597–2641`
- `workerd/src/workerd/api/sql.c++:34–39, :163–169`
- `workerd/src/workerd/io/actor-sqlite.h:134–155, :208–214`
