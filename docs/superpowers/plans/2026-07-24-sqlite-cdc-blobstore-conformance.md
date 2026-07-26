# SQLite CDC BlobStore Conformance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make production SQLite-backed CDC reads obey the formal `BlobStore` verify-on-read contract while preserving indexed subfile work and coherent transactional reads.

**Architecture:** Harden the core `BlobStore` contract, centralize range reconstruction in `leyline-cdc`, and make `leyline-fs` provide only backend-specific transactional storage and indexed manifest selection. The public read path will choose fallback or chunked service inside one SQLite read transaction and will fail closed after choosing chunked service.

**Tech Stack:** Rust 2024, `anyhow`, `blake3`, `rusqlite`, Cargo tests, Taskfile, mutation testing.

## Global Constraints

- Keep Cap'n Proto segment roots, serialized SQLite arena snapshot roots, individual blob hashes, and the SQL projection ABI explicitly distinct.
- SQLite remains the physical chunk backend; do not move payloads to `FsBlobStore`.
- Every selected blob returned by `get(hash)` must satisfy `BLAKE3(bytes) == hash`.
- Preserve indexed `O(overlapping chunks)` selection; never load a file's complete manifest for a range read.
- Freshness, selected-manifest lookup, and blob retrieval must share one SQLite read transaction.
- An integrity failure after selecting the chunked path must return an error without falling back to `nodes.record`.
- No error path may partially modify the caller's destination buffer.
- Manifest authentication and cryptographic `nodes.record` freshness are explicit non-goals.

---

### Task 1: Make the BlobStore Contract Internally Consistent

**Files:**
- Modify: `rs/ll-core/core/src/substrate.rs`
- Modify: `rs/ll-core/core/src/blob_store.rs`

**Interfaces:**
- Consumes: existing `BlobStore::{put,get,contains}` and `ContentAddressed::hash`.
- Produces: explicit physical-existence semantics for `contains`; successful `put` cannot knowingly preserve a corrupt existing value.

- [ ] **Step 1: Write failing existing-entry corruption tests**

Add tests beside the existing `FsBlobStore` and `MemBlobStore` tests:

```rust
#[test]
fn fs_put_rejects_corrupt_existing_entry() {
    let (mut store, _td) = fs_store();
    let bytes = b"canonical";
    let hash = store.put(bytes).unwrap();
    std::fs::write(store.path_for(&hash), b"corrupted").unwrap();

    let err = store.put(bytes).unwrap_err();
    assert!(err.to_string().contains("integrity violation"), "{err:#}");
}

#[test]
fn mem_put_rejects_corrupt_existing_entry() {
    let mut store = MemBlobStore::new();
    let bytes = b"canonical";
    let hash = store.put(bytes).unwrap();
    store.inner.lock().insert(hash, b"corrupted".to_vec());

    let err = store.put(bytes).unwrap_err();
    assert!(err.to_string().contains("integrity violation"), "{err:#}");
}
```

- [ ] **Step 2: Run the focused tests and observe failure**

Run:

```bash
cd rs
cargo test -p leyline-core blob_store::tests::fs_put_rejects_corrupt_existing_entry
cargo test -p leyline-core blob_store::tests::mem_put_rejects_corrupt_existing_entry
```

Expected: both tests fail because `put` currently treats key existence as success without validating the existing bytes.

- [ ] **Step 3: Verify existing entries before returning success**

Change both `put` implementations so the occupied fast path calls `get(hash)` and returns an integrity error on mismatch. Preserve atomic insertion for absent filesystem keys and locked insertion for absent memory keys.

Update `BlobStore` documentation to state:

```rust
/// `contains(h)` reports physical key existence only. It does not verify bytes;
/// only a successful `get(h)` vouches for `σ(bytes) == h`.
///
/// `put(v)` is idempotent for an absent key or a valid existing entry. A
/// corrupt existing entry must be repaired atomically or reported as an error.
```

- [ ] **Step 4: Run the core contract suite**

Run:

```bash
cd rs
cargo test -p leyline-core blob_store
```

Expected: all `BlobStore` tests pass, including the two new corruption cases.

- [ ] **Step 5: Commit**

```bash
git add rs/ll-core/core/src/substrate.rs rs/ll-core/core/src/blob_store.rs
git commit -m "[ley-line-open-ef5c84] fix(core): make BlobStore put fail on corrupt entries"
```

### Task 2: Centralize Checked, Atomic Range Reconstruction

**Files:**
- Modify: `rs/ll-open/cdc/src/lib.rs`

**Interfaces:**
- Consumes: ordered selected `Chunk` spans, total `source_len`, a `BlobStore`, byte offset, and destination buffer.
- Produces: `pub fn read_range_into<S: BlobStore>(chunks: &[Chunk], source_len: usize, store: &S, offset: usize, out: &mut [u8]) -> Result<usize>`.
- Preserves: `read_range` and `reconstruct` as wrappers over the one reconstruction implementation.

- [ ] **Step 1: Add failing atomicity and structural tests**

Add a test store that counts requested hashes and can return corrupt bytes:

```rust
struct ScriptedStore {
    blobs: HashMap<Hash, Vec<u8>>,
    gets: RefCell<Vec<Hash>>,
}

impl BlobStore for ScriptedStore {
    fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
        let hash = bytes.hash();
        self.blobs.insert(hash, bytes.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: Hash) -> Result<Option<Vec<u8>>> {
        self.gets.borrow_mut().push(hash);
        let Some(bytes) = self.blobs.get(&hash) else {
            return Ok(None);
        };
        anyhow::ensure!(bytes.as_slice().hash() == hash, "integrity violation");
        Ok(Some(bytes.clone()))
    }

    fn contains(&self, hash: Hash) -> Result<bool> {
        Ok(self.blobs.contains_key(&hash))
    }
}
```

Cover:

- destination remains sentinel-filled after a selected corrupt blob;
- a non-overlapping corrupt blob is never fetched;
- missing selected blob returns `Err`;
- unordered spans, gap, overlap, zero length, checked-add overflow, and blob/manifest length mismatch return `Err` without panic;
- empty, exact-boundary, cross-boundary, EOF-straddling, wholly-past-EOF, and saturating ranges match authoritative slicing.

- [ ] **Step 2: Run the CDC tests and observe failure**

Run:

```bash
cd rs
cargo test -p leyline-cdc read_range_into
```

Expected: compilation or assertion failure because `read_range_into` and the stronger validation do not exist.

- [ ] **Step 3: Implement one reconstruction function**

Implement `read_range_into` with checked span arithmetic, strict ordering, exact tiling of:

```rust
let wanted_start = offset.min(source_len);
let wanted_end = offset.saturating_add(out.len()).min(source_len);
```

For each overlapping chunk:

1. Require `chunk.offset == previous_chunk_end` where adjacent selected chunks must meet.
2. Fetch with `store.get(chunk.hash)`.
3. Require `blob.len() == chunk.len`.
4. Append only the overlap into a private `Vec<u8>`.
5. Require the private result length equals `wanted_end - wanted_start`.
6. Copy to `out[..result.len()]` only after every check succeeds.

Make `read_range` allocate its result, derive and validate `source_len` from the complete manifest, and delegate to `read_range_into`. Make `reconstruct` continue to delegate to `read_range`.

- [ ] **Step 4: Run focused and mutation gates**

Run:

```bash
cd rs
cargo test -p leyline-cdc
cd ..
task mutants:cdc
```

Expected: all tests pass; no non-timeout survivor changes overlap, ordering, length validation, or atomic-copy behavior.

- [ ] **Step 5: Commit**

```bash
git add rs/ll-open/cdc/src/lib.rs
git commit -m "[ley-line-open-ef5c84] refactor(cdc): centralize checked range reconstruction"
```

### Task 3: Add Transaction-Scoped SqliteBlobStore and Indexed Selection

**Files:**
- Modify: `rs/ll-open/fs/src/chunked.rs`

**Interfaces:**
- Consumes: `rusqlite::Transaction`, `content_chunks`, `content_manifest`, and `content_manifest_meta`.
- Produces:
  - `pub(crate) struct SqliteBlobStore<'tx, 'conn>`.
  - `fn select_range_manifest(tx: &Transaction<'_>, node_id: &str, offset: usize, len: usize) -> Result<Option<ValidatedRangeManifest>>`.
  - `struct ValidatedRangeManifest { chunks: Vec<leyline_cdc::Chunk>, source_len: usize }`.

- [ ] **Step 1: Add failing shared backend and corruption tests**

Inside `chunked.rs` tests, add one generic baseline:

```rust
fn assert_blob_store_baseline<S: BlobStore>(store: &mut S) {
    let bytes = b"shared contract";
    let hash = store.put(bytes).unwrap();
    assert_eq!(store.put(bytes).unwrap(), hash);
    assert!(store.contains(hash).unwrap());
    assert_eq!(store.get(hash).unwrap().unwrap(), bytes);
    assert_eq!(store.get(Hash::ZERO).unwrap(), None);
}
```

Run it against `MemBlobStore`, `FsBlobStore`, and a transaction-scoped `SqliteBlobStore`.

Add SQLite-specific tests that:

- update `chunk_bytes` to same-length different bytes under the original key and require `get` to return an integrity error;
- insert valid bytes under the wrong key and require `get` to return an integrity error;
- prove `contains` remains true for a corrupt physical row;
- prove `put` rejects an already-corrupt key.

- [ ] **Step 2: Run the focused tests and observe failure**

Run:

```bash
cd rs
cargo test -p leyline-fs --no-default-features --features cdc,splice,validate sqlite_blob_store
```

Expected: compilation failure because `SqliteBlobStore` does not exist.

- [ ] **Step 3: Implement SqliteBlobStore**

Define the adapter over `&Transaction<'_>`. Implement:

```rust
fn get(&self, hash: Hash) -> Result<Option<Vec<u8>>> {
    let bytes = self.tx.query_row(
        "SELECT chunk_bytes FROM content_chunks WHERE chunk_hash = ?1",
        params![hash.as_bytes().as_slice()],
        |row| row.get::<_, Vec<u8>>(0),
    ).optional().context("read SQLite blob")?;

    let Some(bytes) = bytes else { return Ok(None) };
    ensure!(bytes.as_slice().hash() == hash, "SqliteBlobStore integrity violation");
    Ok(Some(bytes))
}
```

`put` uses `INSERT OR IGNORE`; if no row was inserted, call `get` before returning success. `contains` performs a key-existence query without hashing.

Route `store_content_manifest_in_transaction` chunk insertion through `SqliteBlobStore::put` while preserving manifest insertion in the caller-owned transaction.

- [ ] **Step 4: Add the indexed selector**

Select only overlapping spans using `OVERLAP_PREDICATE` and:

```sql
ORDER BY byte_offset, seq
```

Select hashes and spans, not `chunk_bytes`. Validate hash width, signed-to-unsigned conversions, non-zero lengths, checked ends, and exact tiling of the requested in-file interval. Return the total source length from the already-fresh metadata witness.

Delete the direct SQL JOIN/copy reconstruction loop. Do not reuse `capture_chunked_content`, because it loads every manifest row.

- [ ] **Step 5: Preserve the subfile work proof**

Extend the existing `EXPLAIN QUERY PLAN` test to target the selector query and require `content_manifest_span`. Add a counting assertion that a 4 KiB range fetches exactly `chunks_touched(...)` blobs and never fetches an intentionally corrupt non-overlapping blob.

- [ ] **Step 6: Run filesystem CDC tests**

Run:

```bash
task test:fs-cdc
```

Expected: unit tests, differential fuzzer, query-plan test, and clippy pass.

- [ ] **Step 7: Commit**

```bash
git add rs/ll-open/fs/src/chunked.rs
git commit -m "[ley-line-open-ef5c84] feat(fs): adapt SQLite CDC storage to BlobStore"
```

### Task 4: Make the Public Read Path Transactionally Coherent

**Files:**
- Modify: `rs/ll-open/fs/src/chunked.rs`
- Modify: `rs/ll-open/fs/src/graph.rs`

**Interfaces:**
- Consumes: public `read_content_at_traced(&Connection, node_id, out, offset)`.
- Produces: the same public signature and `ContentSource`, now with one read transaction and fail-closed chunk integrity.

- [ ] **Step 1: Write the post-open falsifier**

Construct a valid arena, publish its root, open it writable through `SqliteGraphAdapter::from_arena_writable`, then execute a same-length `UPDATE content_chunks SET chunk_bytes = ? WHERE chunk_hash = ?` without changing the key, manifest, size, or mtime.

Call the real `Graph::read_content` path and assert:

```rust
let before = vec![0xA5; 4096];
let mut out = before.clone();
let err = adapter.read_content(&node_id, &mut out, offset).unwrap_err();
assert!(err.to_string().contains("integrity violation"), "{err:#}");
assert_eq!(out, before);
```

Also retain the existing pre-open arena-byte corruption test in `fs/tests/characterization_blake3_sites.rs` as the control for the separate snapshot-integrity domain.

- [ ] **Step 2: Run the activation consumer test and observe failure**

Run:

```bash
cd rs
cargo test -p leyline-fs --no-default-features --features cdc,splice,validate \
  post_open_chunk_corruption_fails_closed
```

Expected: the current path returns corrupted bytes because the graph test can mutate the adapter's private writable connection after the valid arena open.

- [ ] **Step 3: Refactor public selection into one read transaction**

In `read_content_at_traced`:

1. Return `Ok((0, ...))` for an empty destination without opening a transaction.
2. Start `conn.unchecked_transaction()` and perform freshness evaluation inside it.
3. If no fresh manifest exists, read `nodes.record` through the transaction and return `ContentSource::Record`.
4. If fresh, select `ValidatedRangeManifest`, construct `SqliteBlobStore` over the same transaction, and call `leyline_cdc::read_range_into`.
5. Increment the selected source counter only after the read succeeds.
6. Return chunk errors directly; never retry through `nodes.record`.

Update internal helpers to accept `&Transaction<'_>` so the type system prevents accidental statement-level snapshots.

- [ ] **Step 4: Add a barrier-controlled two-connection coherence test**

Use a file-backed WAL database with reader and writer connections:

1. Reader begins a transaction and performs the freshness SELECT.
2. Writer replaces the manifest/chunks and commits.
3. Reader completes selected manifest and blob reads.

Require a coherent old snapshot. A second public read after the first transaction ends must observe the coherent new snapshot. Neither output may combine old and new chunks.

- [ ] **Step 5: Run production CDC gates**

Run:

```bash
task test:fs-cdc
task test:cdc-activation
task mutants:fs
```

Expected: corruption, coherence, activation, query-plan, and mutation gates pass.

- [ ] **Step 6: Commit**

```bash
git add rs/ll-open/fs/src/chunked.rs rs/ll-open/fs/src/graph.rs
git commit -m "[ley-line-open-ef5c84] fix(fs): make CDC range reads transactional and fail closed"
```

### Task 5: Correct the Architecture and Release Contracts

**Files:**
- Modify: `README.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `docs/TABLE_CONTRACT.md`
- Modify: `CHANGELOG.md`
- Modify: `rs/ll-open/fs/src/chunked.rs`
- Create: `tools/check_architecture_vocabulary.sh`
- Create: `tools/test_architecture_vocabulary.sh`
- Modify: `Taskfile.yml`

**Interfaces:**
- Consumes: the four identity definitions approved in the design.
- Produces: one lightweight documentation linter with behavioral fixtures and corrected v0.10.3 release history.

- [ ] **Step 1: Write the failing linter fixture**

Create `tools/test_architecture_vocabulary.sh`. It builds a temporary directory containing `README.md`, `docs/ARCHITECTURE.md`, and `docs/TABLE_CONTRACT.md`, then runs the real linter against controlled inputs.

The broken fixture contains:

```sh
printf '%s\n' \
  'The .db file is the contract' \
  '## The Σ substrate — runtime model' \
  'core tables are the canonical substrate' > "$fixture/docs/ARCHITECTURE.md"
```

Require the linter to exit non-zero and name all three violations. Replace the fixture with the four approved terms and require exit zero:

```text
Cap'n Proto segment root
SQLite arena snapshot root
blob hash
SQL projection ABI
```

The fixture tests the linter's observable exit/output behavior; it does not invoke Cargo.

- [ ] **Step 2: Run the fixture and observe failure**

Run:

```bash
sh tools/test_architecture_vocabulary.sh
```

Expected: failure because the linter script does not exist.

- [ ] **Step 3: Implement the linter and rewrite authority statements**

Create `tools/check_architecture_vocabulary.sh` with an optional repository-root argument defaulting to `.`. It must:

- fail if any of the three forbidden assertions appears;
- require all four approved identity terms across the three documents;
- print each missing or forbidden term and exit non-zero;
- remain POSIX `sh`.

Update the documents so:

- canonical Cap'n Proto bytes define the cross-runtime segment contract;
- `Controller.current_root` identifies one serialized SQLite arena snapshot;
- SQL tables form a local projection/query ABI;
- individual CDC chunks use `BlobStore` hashes and verify-on-read;
- “analysis substrate” and HDC similarity are named as separate systems rather than meanings of Σ.

Update `chunked.rs` module documentation to remove the no-verify exception and describe transaction-scoped verification.

Add `lint:architecture-vocabulary` to `Taskfile.yml`, invoking the script. Wire it into the lightweight docs/change-classification gate created by `ley-line-open-1df0cc`; until that bead lands, include it in `task ci` without adding a Rust dependency to the linter itself.

- [ ] **Step 4: Correct v0.10.3 history**

Change the v0.10.3 changelog statement that says no nested schema tag was published. Record that `clients/go/leyline-schema/v0.10.3` is a content-identical module tag at release commit `a4f57673f0f79d0e3dd8808f19a8b6fc9c5b3347`, published to satisfy public Go module consumers.

- [ ] **Step 5: Run documentation and consumer gates**

Run:

```bash
sh tools/test_architecture_vocabulary.sh
task lint:architecture-vocabulary
task test:cdc-activation
task readme:version-check
task schema:version-check
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/ARCHITECTURE.md docs/TABLE_CONTRACT.md CHANGELOG.md \
  rs/ll-open/fs/src/chunked.rs Taskfile.yml \
  tools/check_architecture_vocabulary.sh tools/test_architecture_vocabulary.sh
git commit -m "[ley-line-open-ef5c1d] docs(architecture): separate substrate identity domains"
```

### Task 6: Final Verification and Patch-Release Handoff

**Files:**
- Verify only: all files changed in Tasks 1–5.
- Update bead evidence: `ley-line-open-ef5c1d`, `ley-line-open-ef5c5a`, `ley-line-open-ef5c84`.

**Interfaces:**
- Consumes: all task commits and repository gates.
- Produces: verified implementation ready for a dedicated patch-release bead.

- [ ] **Step 1: Run formatting and focused gates**

Run:

```bash
task fmt
task test:fs-cdc
task test:cdc-activation
cd rs
cargo test -p leyline-core blob_store
cargo test -p leyline-cdc
cd ..
task lint:architecture-vocabulary
```

Expected: all pass.

- [ ] **Step 2: Run mutation gates**

Run:

```bash
task mutants:cdc
task mutants:fs
```

Expected: no undetected non-timeout mutants in the changed reconstruction or SQLite read path.

- [ ] **Step 3: Run the repository gate**

Run:

```bash
task ci
```

Expected: exit 0.

- [ ] **Step 4: Inspect the final diff and identity language**

Run:

```bash
git diff origin/main...HEAD --check
git diff origin/main...HEAD --stat
rg -n "The \\.db file is the contract|The Σ substrate — runtime model|core tables are the canonical substrate" \
  README.md docs/ARCHITECTURE.md docs/TABLE_CONTRACT.md
```

Expected: clean diff; the final `rg` command returns no matches.

- [ ] **Step 5: Record evidence and create the release bead**

Comment exact command output and commit hashes on the three architecture beads. Create a patch-release bead targeting `v0.10.4` with dependencies on all three, requiring:

- immutable `v0.10.4` source tag;
- matching binary/static-library/header assets;
- `SHA256SUMS` that lists every payload and never itself;
- upload selection that includes `SHA256SUMS`;
- nested Go-module tag policy stated explicitly;
- public `go list -m` resolution;
- an external-publication gate that cannot execute after verifier failure.

Do not publish or move tags from this implementation task.

- [ ] **Step 6: Commit any formatting-only residue**

If `task fmt` changed tracked implementation files, commit only those changes:

```bash
git add rs
git commit -m "[ley-line-open-ef5c84] style(rust): apply CDC conformance formatting"
```

If formatting made no changes, skip this commit.
