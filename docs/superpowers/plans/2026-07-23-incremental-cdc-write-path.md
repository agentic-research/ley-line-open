# Incremental CDC Write Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `SqliteGraphAdapter::write_content` incrementally re-chunk a
small edit from a verified old manifest instead of hashing the whole file.

**Architecture:** Capture the old manifest while its `(size, mtime)` witness
still matches `nodes`, derive edit coordinates from the graph write, then feed
the old spans and new bytes to `leyline_cdc::rechunk_with_stats`. A
table-driven differential harness compares the durable SQLite manifest and
reconstructed bytes against a full-chunk oracle; a traced graph method exposes
work statistics so correct output cannot hide a full scan.

**Tech Stack:** Rust 1.97.1, `rusqlite`, `leyline-cdc`, BLAKE3,
go-task, cargo-mutants.

## Global Constraints

- Use strict RED/GREEN/REFACTOR: record the expected failing test on bead
  `ley-line-open-bd8d33` before each production behavior change.
- Keep `nodes.record` authoritative and preserve the `(size, mtime)` freshness
  gate.
- Do not create chunk tables in a foreign arena.
- Incremental output must equal `leyline_cdc::chunk(new_data)` tuple-for-tuple.
- Do not change SQLite DDL, Cap'n Proto schemas, `leyline-schema`, or wire
  versions.
- Keep the spec and implementation on `fix/ley-line-open-bd8d33` in one PR.
- Use printable deterministic source bytes in graph tests because
  `write_content` intentionally stores `String::from_utf8_lossy`.

______________________________________________________________________

## File Structure

- Modify `rs/ll-open/fs/src/graph.rs`: graph write trace, exact edit
  coordinates, and differential integration harness.
- Modify `rs/ll-open/fs/src/chunked.rs`: verified old-manifest capture,
  shared manifest persistence, full/incremental refresh outcome.
- Modify `CHANGELOG.md`: release-facing statement under `[Unreleased]`.
- Do not modify `rs/ll-open/cdc/src/lib.rs`: `rechunk_with_stats` and its
  exactness/work tests are already shipped and are the primitive being wired.

### Task 1: Establish the RED integration oracle and trace seam

**Files:**

- Modify: `rs/ll-open/fs/src/graph.rs`
- Modify: `rs/ll-open/fs/src/chunked.rs`
- Test: `rs/ll-open/fs/src/graph.rs`

**Interfaces:**

- Produces:

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) enum WriteRefreshOutcome {
      Disabled,
      Skipped,
      Full {
          bytes_scanned: usize,
      },
      Incremental {
          prefix_kept: usize,
          tail_reused: usize,
          rehashed: usize,
          bytes_scanned: usize,
      },
  }

  impl SqliteGraphAdapter {
      fn write_content_traced(
          &self,
          id: &str,
          data: &[u8],
          offset: u64,
      ) -> Result<(usize, WriteRefreshOutcome)>;
  }
  ```

- Temporarily consumes the existing full `refresh_chunked_content`; Task 2
  replaces its implementation.

- [ ] **Step 1: Write the failing deep-edit differential test**

  Add printable deterministic bytes and a manifest oracle to the graph test
  module:

  ```rust
  #[cfg(feature = "cdc")]
  fn cdc_body(seed: u64, len: usize) -> Vec<u8> {
      let mut state = seed | 1;
      (0..len)
          .map(|_| {
              state ^= state << 13;
              state ^= state >> 7;
              state ^= state << 17;
              b'a' + (state % 26) as u8
          })
          .collect()
  }

  #[cfg(feature = "cdc")]
  fn manifest_chunks(
      adapter: &SqliteGraphAdapter,
      node_id: &str,
  ) -> Result<Vec<leyline_cdc::Chunk>> {
      let guard = adapter.writer.lock();
      let mut statement = guard.conn().prepare(
          "SELECT chunk_hash, byte_offset, byte_len
             FROM content_manifest
            WHERE node_id = ?1
            ORDER BY seq",
      )?;
      let rows = statement.query_map([node_id], |row| {
          let hash: Vec<u8> = row.get(0)?;
          let bytes: [u8; 32] = hash.try_into().map_err(|_| {
              rusqlite::Error::FromSqlConversionFailure(
                  0,
                  rusqlite::types::Type::Blob,
                  Box::new(std::io::Error::new(
                      std::io::ErrorKind::InvalidData,
                      "chunk hash is not 32 bytes",
                  )),
              )
          })?;
          Ok(leyline_cdc::Chunk {
              hash: blake3::Hash::from_bytes(bytes),
              offset: row.get::<_, i64>(1)? as usize,
              len: row.get::<_, i64>(2)? as usize,
          })
      })?;
      rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
  }
  ```

  Add the first RED case:

  ```rust
  #[test]
  #[cfg(feature = "cdc")]
  fn graph_write_incrementally_matches_full_chunk_oracle() -> Result<()> {
      let adapter = chunked_adapter()?;
      let node_id = "docs/readme";
      let mut model = cdc_body(0xC0FFEE, 4_000_000);
      adapter.write_content(node_id, &model, 0)?;

      let edit_offset = model.len() / 2;
      let edit = b"XYZ";
      model[edit_offset..edit_offset + edit.len()].copy_from_slice(edit);
      let (_, outcome) =
          adapter.write_content_traced(node_id, edit, edit_offset as u64)?;

      assert_eq!(manifest_chunks(&adapter, node_id)?, leyline_cdc::chunk(&model));
      let mut reconstructed = vec![0; model.len()];
      let read = adapter.read_content(node_id, &mut reconstructed, 0)?;
      assert_eq!(&reconstructed[..read], model);
      match outcome {
          WriteRefreshOutcome::Incremental {
              prefix_kept,
              tail_reused,
              bytes_scanned,
              ..
          } => {
              assert!(prefix_kept > 0);
              assert!(tail_reused > 0);
              assert!(bytes_scanned <= 4 * leyline_cdc::MAX_CHUNK);
          }
          other => panic!("expected incremental refresh, got {other:?}"),
      }
      Ok(())
  }
  ```

- [ ] **Step 2: Run the focused test and record API RED**

  Run:

  ```bash
  cd rs
  cargo test -p leyline-fs --no-default-features \
    --features cdc,splice,validate \
    graph_write_incrementally_matches_full_chunk_oracle
  ```

  Expected: compile failure `E0599` because
  `write_content_traced`/`WriteRefreshOutcome` do not exist. Comment the exact
  failure on `ley-line-open-bd8d33`.

- [ ] **Step 3: Add the minimal trace seam without incremental behavior**

  Move the current `Graph::write_content` body byte-for-byte into the inherent
  `write_content_traced` method. Define `WriteRefreshOutcome` as above. At the
  existing refresh call:

  ```rust
  #[cfg(feature = "cdc")]
  let refresh = match crate::chunked::refresh_chunked_content(
      guard.conn(),
      id,
      new_str.as_ref().as_bytes(),
  )? {
      true => WriteRefreshOutcome::Full {
          bytes_scanned: new_str.len(),
      },
      false => WriteRefreshOutcome::Skipped,
  };
  #[cfg(not(feature = "cdc"))]
  let refresh = WriteRefreshOutcome::Disabled;
  ```

  Return `Ok((data.len(), refresh))`. Make the trait method delegate:

  ```rust
  fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
      self.write_content_traced(id, data, offset)
          .map(|(written, _)| written)
  }
  ```

- [ ] **Step 4: Run the focused test and record semantic RED**

  Run the command from Step 2.

  Expected: the manifest and bytes match, then the test fails with:

  ```text
  expected incremental refresh, got Full { bytes_scanned: 4000000 }
  ```

  Comment this second RED on the bead. This proves the harness distinguishes
  the current full scan from the requested behavior.

- [ ] **Step 5: Commit the RED harness and trace seam**

  ```bash
  git add rs/ll-open/fs/src/graph.rs
  git commit -m \
    "[ley-line-open-bd8d33] test(cdc): expose full-scan write trace"
  ```

### Task 2: Capture a fresh manifest and persist incremental output

**Files:**

- Modify: `rs/ll-open/fs/src/chunked.rs`
- Modify: `rs/ll-open/fs/src/graph.rs`
- Test: `rs/ll-open/fs/src/graph.rs`

**Interfaces:**

- Produces:

  ```rust
  pub(crate) struct ChunkManifestSnapshot {
      chunks: Vec<leyline_cdc::Chunk>,
      source_len: usize,
  }

  pub(crate) enum RefreshOutcome {
      Skipped,
      Full { bytes_scanned: usize },
      Incremental(leyline_cdc::RechunkStats),
  }

  pub(crate) fn capture_chunked_content(
      conn: &Connection,
      node_id: &str,
  ) -> Result<Option<ChunkManifestSnapshot>>;

  pub(crate) fn refresh_chunked_content_after_edit(
      conn: &Connection,
      node_id: &str,
      data: &[u8],
      previous: Option<ChunkManifestSnapshot>,
      edit_offset: usize,
      old_edit_end: usize,
      old_len: usize,
  ) -> Result<RefreshOutcome>;
  ```

- Consumes `leyline_cdc::rechunk_with_stats`.

- [ ] **Step 1: Add snapshot capture with explicit validation**

  Probe for `content_manifest` exactly as existing refresh/invalidation code
  does. Return `Ok(None)` for an absent schema, missing metadata, missing node,
  stale `(size, mtime)`, or no spans. Decode ordered rows and require:

  ```rust
  let mut expected_offset = 0usize;
  for chunk in &chunks {
      anyhow::ensure!(
          chunk.offset == expected_offset,
          "chunk manifest for {node_id} has a gap or overlap at {expected_offset}"
      );
      expected_offset = expected_offset
          .checked_add(chunk.len)
          .context("chunk manifest length overflow")?;
  }
  anyhow::ensure!(
      expected_offset == source_len,
      "chunk manifest for {node_id} covers {expected_offset} bytes, expected {source_len}"
  );
  ```

  A non-32-byte hash and malformed tiling return an error; they are not
  silently treated as a valid incremental base.

- [ ] **Step 2: Extract one atomic manifest writer**

  Refactor `store_content_chunked` so full chunking delegates to:

  ```rust
  fn store_content_manifest(
      conn: &Connection,
      node_id: &str,
      data: &[u8],
      chunks: &[leyline_cdc::Chunk],
  ) -> Result<usize>;
  ```

  Move the existing transaction, delete, chunk/blob inserts, metadata query,
  and commit into that function without changing SQL or ordering.

- [ ] **Step 3: Implement full/incremental refresh**

  ```rust
  pub(crate) fn refresh_chunked_content_after_edit(
      conn: &Connection,
      node_id: &str,
      data: &[u8],
      previous: Option<ChunkManifestSnapshot>,
      edit_offset: usize,
      old_edit_end: usize,
      old_len: usize,
  ) -> Result<RefreshOutcome> {
      if !chunk_schema_present(conn)? {
          return Ok(RefreshOutcome::Skipped);
      }
      let (chunks, outcome) = match previous {
          Some(previous) => {
              anyhow::ensure!(previous.source_len == old_len);
              let (chunks, stats) = leyline_cdc::rechunk_with_stats(
                  &previous.chunks,
                  data,
                  edit_offset,
                  old_edit_end,
                  old_len,
              );
              (chunks, RefreshOutcome::Incremental(stats))
          }
          None => (
              leyline_cdc::chunk(data),
              RefreshOutcome::Full {
                  bytes_scanned: data.len(),
              },
          ),
      };
      store_content_manifest(conn, node_id, data, &chunks)?;
      Ok(outcome)
  }
  ```

  Keep the existing public `store_content_chunked` full-build behavior for
  initial population and direct callers.

- [ ] **Step 4: Wire exact graph edit coordinates**

  Before modifying `content`, capture:

  ```rust
  let old_len = content.len();
  #[cfg(feature = "cdc")]
  let previous =
      crate::chunked::capture_chunked_content(guard.conn(), id)?;
  let write_end = off
      .checked_add(data.len())
      .context("write offset + length overflow")?;
  let edit_offset = off.min(old_len);
  let old_edit_end = write_end.min(old_len);
  ```

  After the `nodes` update, call
  `refresh_chunked_content_after_edit`, map its result to
  `WriteRefreshOutcome`, and preserve the existing reader refresh.

- [ ] **Step 5: Run the focused test and verify GREEN**

  Run:

  ```bash
  cd rs
  cargo test -p leyline-fs --no-default-features \
    --features cdc,splice,validate \
    graph_write_incrementally_matches_full_chunk_oracle
  ```

  Expected: PASS with tuple-for-tuple manifest equality, byte-for-byte
  reconstruction, non-zero prefix/tail reuse, and at most
  `4 * MAX_CHUNK` scanned bytes.

- [ ] **Step 6: Run the existing focused crate gate**

  ```bash
  cd rs
  cargo test -p leyline-fs --no-default-features \
    --features cdc,splice,validate
  ```

  Expected: all tests pass with no warnings.

- [ ] **Step 7: Commit the incremental path**

  ```bash
  git add rs/ll-open/fs/src/chunked.rs rs/ll-open/fs/src/graph.rs
  git commit -m \
    "[ley-line-open-bd8d33] feat(cdc): incrementally refresh writes"
  ```

### Task 3: Expand the differential table and fallback proof

**Files:**

- Modify: `rs/ll-open/fs/src/graph.rs`
- Test: `rs/ll-open/fs/src/graph.rs`

**Interfaces:**

- Consumes `write_content_traced`, `manifest_chunks`, and the byte-vector
  oracle from Tasks 1–2.

- Produces one table-driven edit harness and one fallback table.

- [ ] **Step 1: Convert the deep edit into a case table**

  Define:

  ```rust
  struct WriteCase {
      name: &'static str,
      offset: usize,
      bytes: Vec<u8>,
  }
  ```

  Run sequential cases against the same adapter/model:

  ```rust
  let cases = vec![
      WriteCase {
          name: "deep overwrite",
          offset: model.len() / 2,
          bytes: b"XYZ".to_vec(),
      },
      WriteCase {
          name: "equal length across likely boundaries",
          offset: leyline_cdc::MAX_CHUNK - 2,
          bytes: b"boundary".to_vec(),
      },
      WriteCase {
          name: "append at eof",
          offset: model.len(),
          bytes: b"append".to_vec(),
      },
      WriteCase {
          name: "write beyond eof",
          offset: model.len() + 97,
          bytes: b"tail".to_vec(),
      },
      WriteCase {
          name: "empty write",
          offset: model.len() / 3,
          bytes: Vec::new(),
      },
  ];
  ```

  Apply the graph's exact model semantics before each real write:

  ```rust
  let end = case.offset.checked_add(case.bytes.len()).unwrap();
  if end > model.len() {
      model.resize(end, 0);
  }
  model[case.offset..end].copy_from_slice(&case.bytes);
  ```

  Assert the manifest oracle and reconstructed bytes after every row.

- [ ] **Step 2: Run each new row RED before coordinate fixes**

  Add rows one at a time and run the named test after each addition. Record any
  newly exposed coordinate failure on the bead before changing production
  code. Expected final state: every case reports `Incremental`, except a
  semantically unchanged empty write may report incremental work with all
  chunks reused.

- [ ] **Step 3: Add the fallback table RED**

  Cover:

  ```text
  schema present + no manifest => Full
  schema present + stale witness => Full
  no chunk schema               => Skipped
  ```

  For the stale case, populate a valid manifest then update only
  `nodes.mtime` through SQL before `write_content_traced`. Assert the full
  fallback still produces the oracle manifest and bytes.

- [ ] **Step 4: Implement only fallback corrections exposed by RED**

  Keep missing/stale snapshots as `None`, preserve malformed-manifest errors,
  and leave foreign schemas untouched. Do not add branches that no harness row
  requires.

- [ ] **Step 5: Run the full focused gate**

  ```bash
  task test:fs-cdc
  ```

  Expected: tests and `-D warnings` clippy pass.

- [ ] **Step 6: Commit the expanded harness**

  ```bash
  git add rs/ll-open/fs/src/graph.rs rs/ll-open/fs/src/chunked.rs
  git commit -m \
    "[ley-line-open-bd8d33] test(cdc): differential write matrix"
  ```

### Task 4: Changelog and release-grade verification

**Files:**

- Modify: `CHANGELOG.md`
- Verify: `Taskfile.yml`

**Interfaces:**

- Documents bead `ley-line-open-bd8d33`.

- Does not change package or schema versions.

- [ ] **Step 1: Update `[Unreleased]`**

  Add:

  ```markdown
  ### Changed

  - **Incremental CDC writes** (`ley-line-open-bd8d33`) — chunk-backed graph
    writes now capture a freshness-verified old manifest and call
    `rechunk_with_stats`, so a small edit hashes only its bounded resync window
    instead of the whole file. A differential graph harness requires the
    durable manifest and reconstructed bytes to equal a full rechunk after
    overwrite, append, beyond-EOF, boundary, and empty-write cases. No
    `leyline-schema` or wire-format change.
  ```

- [ ] **Step 2: Run formatting and focused tests**

  ```bash
  cargo fmt --manifest-path rs/Cargo.toml --all -- --check
  mdformat --check \
    docs/superpowers/specs/2026-07-23-incremental-cdc-write-path-design.md \
    docs/superpowers/plans/2026-07-23-incremental-cdc-write-path.md \
    CHANGELOG.md
  task test:fs-cdc
  ```

  Expected: all commands pass.

- [ ] **Step 3: Run the complete Taskfile CI gate**

  ```bash
  task ci
  ```

  Expected: exit 0, including `check:all-features`.

- [ ] **Step 4: Run both mutation gates**

  ```bash
  task mutants:cdc
  task mutants:fs
  ```

  Expected: no missed mutants. Exit 3 remains accepted only by the existing
  `mutants:cdc` Taskfile wrapper for detected non-terminating mutants.

- [ ] **Step 5: Run configured commit/push gates**

  ```bash
  task check:commit
  git status --short
  ```

  Expected: `task check:commit` passes and only intended files are modified.
  The configured pre-push hook will run exact `task ci` again during push.

- [ ] **Step 6: Commit documentation**

  ```bash
  git add CHANGELOG.md \
    docs/superpowers/plans/2026-07-23-incremental-cdc-write-path.md
  git commit -m \
    "[ley-line-open-bd8d33] docs(cdc): record incremental writes"
  ```

- [ ] **Step 7: Push and open the same-branch PR**

  ```bash
  git push -u origin fix/ley-line-open-bd8d33
  gh pr create \
    --base main \
    --head fix/ley-line-open-bd8d33 \
    --title "feat(cdc): incrementally rechunk graph writes" \
    --body-file /tmp/ley-line-open-bd8d33-pr.md
  ```

  Expected: one ready PR containing the design, plan, implementation, tests,
  and changelog. Monitor exact `task ci` and mutation checks to terminal green
  before merge.
