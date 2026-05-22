# Cache substrate — status checkpoint (2026-05-22, updated after Phase 3)

Branch: `feat/cache-schema-ae89aa` (LLO) + `feat/portable-cache-aeb262` (mache, via worktree)
Trigger: `/loop 5m /evolve` ran 12 iterations under user thoroughness
directive ("don't do the minimum; do the opposite"). The mache
portable-cache feature is now **Phases 1+2+3 complete**. The mache
checkout was unblocked via a `git worktree` at
`~/.rsry/worktrees/mache/portable-cache-aeb262/` so the parallel
work on `infra/elixir-parser-out-of-lfs` could continue unaffected.

This doc is the resume-after-context-loss artifact. If you arrive
here on a different machine, after compaction, or weeks later, the
sections below tell you exactly what shipped, what's tested, and
what's left.

## The dep chain

```
✅ ley-line-open-ae89aa   schema + bindings + cross-runtime fixtures + cross-repo gate
✅ ley-line-open-bb0316   FsBlobStore + MemBlobStore + race-fix + sweep API
✅ cloister-bb168f         build-cache/v1 spec + conformance vectors + producer
✅ mache-aeb262 Phase 1    `mache cache push` (emit lockfile + chunks)
✅ mache-aeb262 Phase 2    `mache cache pull --verify` (local-CAS restore)
✅ mache-aeb262 Phase 3    `--remote` push/pull via OCI build-cache/v1
✅ mache-aeb262 Phase 5    cache-roundtrip CI workflow + task entries
⏭ mache-aeb262 Phase 4    chunks-as-parse-outputs (sheaf-driven incremental)
```

## Commits on `feat/cache-schema-ae89aa` (oldest first)

| SHA | Bead | Summary |
|---|---|---|
| `b60a58c` | ae89aa | design(adr): ADR-0021 CacheLockfile as substrate primitive |
| `32c7e56` | ae89aa | feat(schema): cache.capnp + Rust + Go bindings + 12 round-trip tests |
| `ba5c351` | bb0316 | feat(blob-store): FsBlobStore + MemBlobStore impls |
| `752d743` | ae89aa | test(cache): cross-runtime fixtures extend T8.10 to cache.capnp |
| `ac97a07` | ae89aa | feat(schema-capnp): example binary gen_build_cache_vectors |
| `3cf2d99` | ae89aa | chore(deps): Cargo.lock updates from new dev-deps |
| `ccc383b` | ae89aa | test(cache): cross-repo conformance gate for cloister-spec vectors |
| `1fffd67` | bb0316 | fix(blob-store): unique temp-file name (race-safe under concurrent putters) |
| `5cce387` | bb0316 | feat(blob-store): FsBlobStore::sweep_stale_temps for orphan cleanup |

## Commits on `cloister` `main`

| SHA | Bead | Summary |
|---|---|---|
| `83d5fca` | baac45 | design(seams): Ring Seam + Portable Units companion design drafts |
| `a36bec2` | bb168f | design(spec): cloister-spec/build-cache/v1 — OCI-shaped transport |
| `f903d64` | bb168f | feat(spec): cloister-spec/build-cache/v1 conformance vectors |

## Test inventory

Run from `rs/` unless noted.

| Crate / file | Tests | What it pins |
|---|---|---|
| `leyline-core` (`blob_store` module) | 28 | FsBlobStore + MemBlobStore: round-trip, idempotency, absence, corruption detection, layout shape, prefix collision, large blobs, persistence, constructor discipline, concurrent put (same / distinct / interleaved with get), stale-temp sweep (6 cases) |
| `leyline-schema-capnp` lib | 8 | cache.capnp Rust round-trip on every struct + Position/Range siblings |
| `leyline-schema-capnp` `cross_runtime_fixtures.rs` | 5 (cache half) | Byte-equal canonical encoding for minimal + realistic CacheLockfile + decode + size invariant |
| `leyline-schema-capnp` `fileid_allowlist.rs` | 1 | cache.capnp's fileId is in the allowlist |
| `leyline-schema-capnp` `build_cache_vectors_consistency.rs` | 4 | cloister-spec vectors decode + every chunk hashes correctly + manifest digests match + sha256sum verifies + digests.json self-consistent |
| `clients/go/leyline-schema/cache` (Go) | 6 | Cross-runtime decode of the same fixtures the Rust side asserts byte-equal |

**Cache-related tests: 52. Full leyline-core tests: 62. All pass on this branch.**

## Verification commands

```bash
# 1. Rust unit + lib tests
cd ~/remotes/art/ley-line-open/rs
cargo test -p leyline-core                                # 62 / 62
cargo test -p leyline-schema-capnp                        # all
cargo test -p leyline-schema-capnp --test cross_runtime_fixtures
cargo test -p leyline-schema-capnp --test build_cache_vectors_consistency

# 2. Regenerate conformance vectors (deterministic)
cargo run -p leyline-schema-capnp --example gen_build_cache_vectors -- \
    ../../cloister/cloister-spec/build-cache/v1/vectors

# 3. Self-verify the committed vectors
cd ~/remotes/art/cloister/cloister-spec/build-cache/v1/vectors
sha256sum -c VECTORS.sha256

# 4. Go binding tests
cd ~/remotes/art/ley-line-open/clients/go/leyline-schema
GOWORK=off go test ./cache/                               # 6 / 6

# 5. Full LLO Taskfile gate (includes FUSE deps)
cd ~/remotes/art/ley-line-open
task test
```

## What's actually in the substrate

For a new consumer (mache, me-bundle, agent-corpus) the surface is:

### Rust

```rust
use leyline_core::{FsBlobStore, MemBlobStore, BlobStore, Hash, ContentAddressed};
use leyline_schema_capnp::cache_capnp::cache_lockfile;

// Open a content-addressed blob store at <root>/objects/
let mut store = FsBlobStore::open(arena_dir.join("objects"))?;

// Idempotent put. Same bytes → same hash → no duplicate IO.
let h = store.put(&bytes)?;

// Verify-on-read get. Mismatch on disk is an Err, not a panic.
let bytes = store.get(h)?;

// Periodic maintenance.
let report = store.sweep_stale_temps(Duration::from_secs(3600));
// report.removed() / report.errors / report.is_clean()

// Build a CacheLockfile via capnp Builder (see schema for shape).
// Canonical-encode for wire shipment; decode via cache_lockfile::Reader.
```

### Go

```go
import (
    capnp "capnproto.org/go/capnp/v3"
    cache "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/cache"
)

bytes, _ := os.ReadFile("manifest-config.bin")
msg, _ := capnp.Unmarshal(bytes)
lf, _ := cache.ReadRootCacheLockfile(msg)
// Walk lf.Sources(), lf.Topology(), lf.Root().
```

### Spec

`cloister-spec/build-cache/v1/` — capability interface (README + 4 wire docs + 6-file vector bundle). Conformance criteria for providers + consumers documented. Vectors are deterministic + self-verifying.

## What still needs to be done (consumer side)

### `mache-aeb262` Phase 1 — `mache push` emits lockfile

Once mache's branch is free:

1. `cd ~/remotes/art/mache && git checkout feat/portable-cache-aeb262`
2. Add a `clients/go/leyline-schema` dep in mache's go.mod pointing at the LLO Go module
3. Add a `cmd/cache.go` with `mache push <out-dir>` that:
   - Walks `mache.db`'s `_source` and `_ast` tables
   - For each source: compute BLAKE3 of the original bytes + emit a chunk file (the capnp-encoded parse result)
   - Build a `CacheLockfile` referencing the chunks
   - Write `mache.lock.toml` + chunks into `<out-dir>`
4. Test: round-trip via mache pull (Phase 2) using `FsBlobStore` as the local CAS

### `mache-aeb262` Phase 2 — `mache pull` from local CAS

1. `mache pull --from-local <cas-path>` reads a `mache.lock.toml`
2. For each `sources[]`: fetch chunk from local CAS via `FsBlobStore::get`
3. Reassemble the db, verify the root hash matches `lockfile.root`

### `mache-aeb262` Phase 3 — remote build-cache transport

1. Implement `build-cache/v1` consumer per `cloister-spec/build-cache/v1/wire/`
2. `mache push --to <registry-url>` pushes chunks via OCI blob upload + publishes the manifest
3. `mache pull --from <registry-url>` does the inverse

The spec is fully written; the test vectors prove the digest semantics; the only thing left is the HTTP+OCI plumbing.

## Mache ADR-0020 needs a path correction

`mache/docs/adr/0020-portable-cache-lockfile-schema.md` (commit
`3733813`) was written before the substrate-side correction that
moved `cache.capnp` from `public-schema/capnp/` to
`schema-capnp/schemas/`. The text still references the old path in
a few places. Should be fixed when the mache branch is free; not
load-bearing (it's a consumer-side doc; the schema's actual location
is what consumers consume).

## Architectural corrections caught during the work

1. **Schema location** — cache.capnp belongs in `schema-capnp/schemas/`
   (structural substrate), not `public-schema/capnp/` (protocol RPC).
   ADR-0021 updated.

2. **Hash shape** — `common.Hash` exists, 32-byte BLAKE3-locked per
   Σ §3.4. Initial ADR proposed `Hash { algo, bytes }` for SHA-*
   future-proofing; that contradicted the substrate's intentional
   lock. Removed. ADR-0021 updated.

3. **Concurrent-put race** — initial `FsBlobStore::put` had a temp-
   file name collision when same-process same-content racers hit
   `create_new(true)` (O_EXCL) with identical `(pid, hash)` names.
   Found by `fs_concurrent_put_same_content_is_safe` stress test.
   Fixed with a process-wide `AtomicU64` nonce in the temp name.

4. **Orphan temp files** — the race fix produces orphan temp files
   (one rename wins; the others' temp files are left over). Added
   `FsBlobStore::sweep_stale_temps(threshold)` as the cleanup path.

## Why this work was worth doing under blocked-consumer conditions

The user's thoroughness directive was "do the opposite of minimum;
we don't want to come back to this code." Each iteration added
either coverage or hardening:

- Iteration 1: schema + chunk store impl. Phase 0 of the chain.
- Iteration 2: cross-runtime fixtures. Made the contract auditable
  across Rust + Go simultaneously.
- Iteration 3: conformance vectors + spec wire docs. Made the
  contract auditable by anyone building a third-party impl.
- Iteration 4: cross-repo conformance gate. Made the COMMITTED
  vectors hand-edit-proof.
- Iteration 5: concurrent-put race fix. Found a real bug that
  would have shipped under "minimum needed."
- Iteration 6 (just now): orphan-sweep. Closed the loop on the
  race fix.
- Iteration 7 (this doc): checkpoint. Makes the work resumeable.

When the mache branch frees up, Phase 1 will land against a
substrate that's been through 6 rounds of adversarial testing and
documentation. The "we don't want to come back to this code"
directive was the right call.

## Cron status

`*/5 * * * *` job `3d6c97d9`. Auto-expires after 7 days from
2026-05-22 ~02:00. To cancel: `CronDelete 3d6c97d9`. After this
checkpoint, the marginal return per iteration drops sharply
(no more obvious substrate gaps to close); recommended next step
is to `CronDelete` and resume when the mache branch is free OR
when the user wants to start me-bundle (`ley-line-open-dffb77`,
the other major consumer of CacheLockfile).
