//! Chunk-backed content storage — "CDC on the SQL".
//!
//! ## Why this exists
//!
//! `SqliteGraphAdapter::read_content` serves a range read by `SELECT record`
//! (loading the **entire** file) and then slicing. A 4 KiB mount read of a
//! 100 MB file therefore materializes 100 MB. That is a storage-shape problem,
//! not a reader problem — so the fix belongs at the SQL layer.
//!
//! This is ADR-0026's thesis ("the SQL projection should be a lightweight index
//! into content-addressed blobs, never re-materialize") applied at **chunk**
//! granularity, using the same arena-local blob-table pattern as
//! `source_blobs` / `capnp_blobs` so an arena stays a single portable `.db`.
//!
//! ## The shape
//!
//! - `content_chunks` — content-addressed chunk store (`σ = BLAKE3` → bytes),
//!   shared across every file, so identical chunks are stored once.
//! - `content_manifest` — per-node ordered spans into `content_chunks`.
//!
//! A range read becomes a **SQL `WHERE` clause** over the manifest: only the
//! rows whose span overlaps `[offset, offset+len)` are selected, and only those
//! chunks' bytes are read. The database never touches the unrequested chunks —
//! that is the materialize-on-read property, enforced by the query itself.
//!
//! Chunking is [`leyline_cdc`] (HuggingFace `gearhash` CDC with xet's
//! parameters), so boundaries are content-defined: an edit changes only the
//! chunks in its own region, and unchanged chunks keep their identity — so an
//! edit re-*stores* O(1) chunks rather than O(file).
//!
//! The graph write path captures a freshness-verified old manifest before
//! changing `nodes.record`, then calls `leyline_cdc::rechunk_with_stats` with
//! the exact overwrite coordinates. A small edit therefore hashes only its
//! bounded resync window and stores only its new chunks. Initial population,
//! a missing manifest, or a stale freshness witness deliberately falls back to
//! a full chunk.
//!
//! ## Public surface — one way in
//!
//! [`read_content_at`] / [`read_content_at_traced`] are the ONLY public readers.
//! The raw readers are `pub(crate)` on purpose: they bypass the freshness gate
//! in [`has_chunked_content`], and that gate is what keeps a missed
//! invalidation from becoming silent data corruption. Making the unsafe path
//! unreachable is stronger than documenting that it exists.
//!
//! ## What this does NOT do (stated so the claims stay honest)
//!
//! - **No verify-on-read.** `leyline_cdc::read_range` fetches through the
//!   `BlobStore` trait, whose contract σ-verifies returned bytes. This path
//!   reads `chunk_bytes` straight out of SQLite and trusts them. Chunk hashes
//!   are still content-addressed *identity* (that is what makes dedup and
//!   boundary stability work), but they are not re-checked on each read, so
//!   this path does not by itself detect tampering with the `.db`. Integrity
//!   at rest comes from arena-root verification (`verify_arena_root` in this
//!   crate's `lib.rs`), not from a per-chunk check here. Adding one is cheap
//!   (BLAKE3 is ~10x faster than the SQLite read it would follow) if a threat
//!   model ever wants defense in depth below the arena root.
//! - **No garbage collection.** Nothing deletes from `content_chunks`. Storing
//!   a node repeatedly retains every chunk any version ever had — content
//!   addressing means re-storing identical data is free, but genuinely changed
//!   regions accumulate. Fine for a write-once projection; a reachability
//!   sweep (`DELETE FROM content_chunks WHERE chunk_hash NOT IN (SELECT
//!   chunk_hash FROM content_manifest)`) is needed before this backs a
//!   long-lived mount write path.
//! - **The manifest is a derived index, not the source of truth.**
//!   `nodes.record` remains authoritative — it is the cross-runtime contract
//!   (`leyline-schema`: "mache writes it, leyline-fs reads it"). The manifest
//!   accelerates reads and MUST be invalidated whenever `record` changes
//!   behind this crate's back. `leyline-ts`'s splice/reproject does exactly
//!   that, so `graph.rs` invalidates after both `flush_node` and
//!   `batch_splice`. A writer outside this crate that updates `record` while
//!   leaving a manifest in place would serve stale bytes — that is the
//!   invariant to preserve when adding write paths.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::atomic::{AtomicU64, Ordering};

/// Chunk store + per-node manifest. Mirrors the `source_blobs` shape so
/// everything durable stays inside the one `.db`.
pub const CONTENT_CHUNKS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS content_chunks (
    chunk_hash  BLOB PRIMARY KEY,
    chunk_bytes BLOB NOT NULL,
    -- Named chunk_len, not byte_len: the manifest's byte_len joins against
    -- this table, and a shared name makes the join predicate ambiguous.
    chunk_len   INTEGER GENERATED ALWAYS AS (length(chunk_bytes)) STORED
);

CREATE TABLE IF NOT EXISTS content_manifest (
    node_id    TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    chunk_hash BLOB    NOT NULL,
    byte_offset INTEGER NOT NULL,
    byte_len    INTEGER NOT NULL,
    PRIMARY KEY (node_id, seq)
);

-- The index that makes a range read a WHERE clause rather than a full scan.
CREATE INDEX IF NOT EXISTS content_manifest_span
    ON content_manifest(node_id, byte_offset);

-- Freshness witness: the (size, mtime) of the `nodes` row this manifest was
-- built from. A read compares it against the row's CURRENT values, so a
-- manifest whose source moved on is REFUSED rather than served. This is what
-- makes a missed invalidation degrade to slow-but-correct instead of silently
-- wrong. See `has_chunked_content`.
CREATE TABLE IF NOT EXISTS content_manifest_meta (
    node_id      TEXT PRIMARY KEY,
    source_len   INTEGER NOT NULL,
    source_mtime INTEGER
);";

/// Create the chunk store + manifest tables (idempotent).
pub fn create_chunked_content_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CONTENT_CHUNKS_DDL)
        .context("create chunked content schema")
}

#[derive(Debug)]
pub(crate) struct ChunkManifestSnapshot {
    chunks: Vec<leyline_cdc::Chunk>,
    source_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    Skipped,
    Full { bytes_scanned: usize },
    Incremental(leyline_cdc::RechunkStats),
}

fn chunk_schema_present(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_manifest'",
        [],
        |_| Ok(true),
    )
    .optional()
    .context("probe for content_manifest")
    .map(|present| present.unwrap_or(false))
}

/// Store `data` for `node_id` as content-defined chunks, replacing any existing
/// manifest. Chunk bytes are `INSERT OR IGNORE`d, so a chunk shared with another
/// file (or an earlier version of this one) costs nothing. Returns the chunk
/// count.
pub fn store_content_chunked(conn: &Connection, node_id: &str, data: &[u8]) -> Result<usize> {
    let chunks = leyline_cdc::chunk(data);
    store_content_manifest(conn, node_id, data, &chunks)
}

fn store_content_manifest(
    conn: &Connection,
    node_id: &str,
    data: &[u8],
    chunks: &[leyline_cdc::Chunk],
) -> Result<usize> {
    // Atomic, and it must be. The DELETE + per-chunk INSERT loop is only a
    // valid manifest at the end: interrupt it partway and the node's spans no
    // longer tile [0, len), which `read_content_chunked` cannot detect — it
    // copies whatever spans exist to their absolute offsets, so a missing span
    // reads back as stale buffer bytes rather than an error. Silent wrong data
    // is the worst failure mode available here, so the write is all-or-nothing.
    //
    // `unchecked_transaction` because the API takes `&Connection` (matching
    // `Graph`'s shape) rather than `&mut`; the caller must not already be in a
    // transaction on this connection.
    let tx = conn
        .unchecked_transaction()
        .context("begin chunked store transaction")?;
    tx.execute(
        "DELETE FROM content_manifest WHERE node_id = ?1",
        params![node_id],
    )
    .context("clear previous manifest")?;

    let mut put_chunk = tx
        .prepare("INSERT OR IGNORE INTO content_chunks (chunk_hash, chunk_bytes) VALUES (?1, ?2)")
        .context("prepare chunk insert")?;
    let mut put_span = tx
        .prepare(
            "INSERT INTO content_manifest (node_id, seq, chunk_hash, byte_offset, byte_len) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .context("prepare manifest insert")?;

    for (seq, c) in chunks.iter().enumerate() {
        let bytes = &data[c.offset..c.offset + c.len];
        put_chunk
            .execute(params![c.hash.as_bytes().as_slice(), bytes])
            .context("insert chunk")?;
        put_span
            .execute(params![
                node_id,
                seq as i64,
                c.hash.as_bytes().as_slice(),
                c.offset as i64,
                c.len as i64
            ])
            .context("insert manifest span")?;
    }
    drop(put_chunk);
    drop(put_span);

    // Capture the freshness witness inside the SAME transaction as the
    // manifest, so the two can never disagree. `source_mtime` is NULL when the
    // node has no `nodes` row (pure content-addressed use, e.g. tests driving
    // this layer directly) — `has_chunked_content` refuses those for
    // `nodes`-backed reads anyway.
    // No `unwrap_or(None)` here. Swallowing an error would store a NULL
    // witness, which reads as "never fresh" — correct-ish, but it silently
    // demotes every future read of this node to the slow path with no signal.
    // Worse, it is the exact shape of the verify-fallback smell ley-line hit in
    // `receiver.rs` (ley-line-1d7194): catch ANY error, quietly continue in a
    // weaker mode. A missing `nodes` row is expected and handled; a failing
    // query is a real fault and must surface.
    // Two benign cases, both probed for EXPLICITLY rather than caught: the
    // arena has no `nodes` table at all (pure content-addressed use), or the
    // node has no row yet. Everything else is a real fault and propagates.
    let nodes_table: bool = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for nodes table")?
        .unwrap_or(false);
    let node_meta: Option<(i64, i64)> = if nodes_table {
        tx.query_row(
            "SELECT size, mtime FROM nodes WHERE id = ?1",
            params![node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .context("read node freshness witness")?
    } else {
        None
    };
    tx.execute(
        "INSERT OR REPLACE INTO content_manifest_meta (node_id, source_len, source_mtime) \
         VALUES (?1, ?2, ?3)",
        params![
            node_id,
            data.len() as i64,
            node_meta.map(|(_, mtime)| mtime)
        ],
    )
    .context("record manifest freshness witness")?;

    tx.commit().context("commit chunked store")?;
    Ok(chunks.len())
}

/// The overlap predicate, defined once. `?1` = node id, `?2` = range end,
/// `?3` = range start, `?4` = the seek floor (see below). Every path that
/// selects chunks for a range MUST use this string — a second hand-written
/// copy is how an off-by-one slips in (a copy in a test drifts silently from
/// the shipped query).
///
/// ## Why `?4` exists
///
/// `(byte_offset + byte_len) > ?3` is the true lower bound, but it is an
/// expression over two columns, so SQLite cannot use it as an index bound —
/// it becomes a post-filter. With only `byte_offset < ?2` bounding the seek,
/// a read near the END of a large file walks every index entry from offset 0
/// forward and discards them: O(chunks before the range), not O(overlapping
/// chunks). `EXPLAIN QUERY PLAN` shows this plainly.
///
/// `byte_offset >= ?4` restores a real range seek. It is sound because CDC
/// clamps every chunk to at most [`leyline_cdc::MAX_CHUNK`] bytes: an
/// overlapping chunk satisfies `byte_offset + byte_len > start` and
/// `byte_len <= MAX_CHUNK`, hence `byte_offset > start - MAX_CHUNK`. Passing
/// the weaker `>=` with a saturating subtraction keeps offset 0 included.
/// It therefore cannot exclude an overlapping chunk — and the exactness test
/// plus the fuzzer's oracle check that empirically, not just by argument.
const OVERLAP_PREDICATE: &str = "node_id = ?1 AND byte_offset >= ?4 AND byte_offset < ?2 \
     AND (byte_offset + byte_len) > ?3";

/// Lower bound for the index seek — see [`OVERLAP_PREDICATE`]. No chunk
/// starting before this point can reach `start`, because CDC caps chunk length
/// at `MAX_CHUNK`.
fn seek_floor(start: usize) -> i64 {
    start.saturating_sub(leyline_cdc::MAX_CHUNK) as i64
}

/// How many chunks a read of `len` bytes at `offset` would touch. This is the
/// cost of the read, in chunks — the number the whole design exists to keep
/// small. Uses [`OVERLAP_PREDICATE`], so it measures the shipped selection.
pub fn chunks_touched(conn: &Connection, node_id: &str, offset: u64, len: usize) -> Result<usize> {
    let start = offset as usize;
    let end = start.saturating_add(len);
    let sql = format!("SELECT COUNT(*) FROM content_manifest WHERE {OVERLAP_PREDICATE}");
    let n: i64 = conn
        .query_row(
            &sql,
            params![node_id, end as i64, start as i64, seek_floor(start)],
            |r| r.get(0),
        )
        .context("count touched chunks")?;
    Ok(n as usize)
}

/// Read `buf.len()` bytes at `offset` for `node_id`, touching **only** the
/// chunks whose span overlaps the request.
///
/// **Deliberately not public.** This reads the manifest UNCHECKED — it does not
/// consult [`has_chunked_content`], so it will happily serve a stale manifest.
/// The freshness gate is the only thing standing between a missed invalidation
/// and silent data corruption (a deleted file's bytes surfacing in a new file
/// at the same path), and a `pub` unchecked reader is an open invitation to
/// route around it. [`read_content_at`] is the entry point; the compiler now
/// enforces that rather than a doc comment asking nicely.
pub(crate) fn read_content_chunked(
    conn: &Connection,
    node_id: &str,
    buf: &mut [u8],
    offset: u64,
) -> Result<usize> {
    let start = offset as usize;
    let end = start.saturating_add(buf.len());
    if buf.is_empty() {
        return Ok(0);
    }

    // The load-bearing query: overlap predicate in SQL, so unrequested chunks
    // are never read off disk.
    // No table alias: the predicate's columns live only on content_manifest,
    // so OVERLAP_PREDICATE drops in verbatim — one definition, no drift.
    //
    // No ORDER BY: each chunk is copied to its own absolute position in `buf`,
    // so the result does not depend on row order — and the manifest tiles the
    // file without gaps (fuzzer invariant 1), so every byte of the range is
    // covered exactly once regardless. Sorting here would be a real cost
    // (SQLite can walk content_manifest_span directly) bought with nothing.
    // Verified by mutation: adding/removing an ORDER BY changes no test.
    let sql = format!(
        "SELECT byte_offset, byte_len, chunk_bytes \
           FROM content_manifest \
           JOIN content_chunks USING (chunk_hash) \
          WHERE {OVERLAP_PREDICATE}"
    );
    let mut stmt = conn.prepare(&sql).context("prepare range read")?;

    let rows = stmt
        .query_map(
            params![node_id, end as i64, start as i64, seek_floor(start)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as usize,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .context("run range read")?;

    let mut written = 0usize;
    for row in rows {
        let (c_off, c_len, bytes) = row.context("decode chunk row")?;
        let c_end = c_off + c_len;
        let lo = start.saturating_sub(c_off); // first wanted byte within chunk
        let hi = end.min(c_end) - c_off; // last wanted byte within chunk
        let src = &bytes[lo..hi];
        let dst = (c_off + lo) - start;
        buf[dst..dst + src.len()].copy_from_slice(src);
        written = written.max(dst + src.len());
    }
    Ok(written)
}

/// Drop `node_id`'s chunk manifest, so subsequent reads fall back to
/// `nodes.record`.
///
/// This is the safety valve for writers this crate does not control.
/// `leyline-ts`'s splice/reproject updates `nodes.record` directly (see
/// `leyline_ts::splice::reproject_source`), and it knows nothing about chunk
/// tables. A manifest left behind after such a write describes the OLD
/// content, and `read_content_chunked` would serve those stale bytes happily —
/// silently wrong data, the worst outcome available. Invalidating is cheap and
/// degrades to the slow-but-correct path; repopulating is the caller's choice
/// once the new content is known.
///
/// Chunk BYTES are deliberately left in `content_chunks`: they are
/// content-addressed, so they cost nothing to keep and are immediately reused
/// if the same content reappears. See the module docs on garbage collection.
pub(crate) fn invalidate_chunked_content(conn: &Connection, node_id: &str) -> Result<()> {
    // A foreign arena has no chunk tables at all — nothing to invalidate, and
    // probing must not turn into an error on that path.
    let table_present: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_manifest'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for content_manifest")?
        .unwrap_or(false);
    if !table_present {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM content_manifest WHERE node_id = ?1",
        params![node_id],
    )
    .context("invalidate chunk manifest")?;
    Ok(())
}

/// Invalidate `node_id` **and every descendant path** (`node_id/...`).
///
/// Node ids are paths, and the writers that delete or rename a node cascade
/// over descendants with `id LIKE 'node_id/%'`. Invalidation has to cascade
/// identically, or a child's manifest outlives its `nodes` row.
///
/// This is the cross-generation-leak guard: because ids are paths and paths
/// get REUSED, an orphaned manifest is not merely stale — it is attached to
/// whatever node is created at that path next, and `has_chunked_content` will
/// happily serve the previous occupant's bytes to a brand-new file that was
/// never written. Verified: without this, `write_content(p, "secret")` →
/// `remove_node(p)` → `create_node(p)` → `read_content(p)` returns "secret".
pub(crate) fn invalidate_chunked_content_subtree(conn: &Connection, node_id: &str) -> Result<()> {
    let table_present: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_manifest'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for content_manifest")?
        .unwrap_or(false);
    if !table_present {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM content_manifest WHERE node_id = ?1 OR node_id LIKE ?2",
        params![node_id, format!("{node_id}/%")],
    )
    .context("invalidate chunk manifest subtree")?;
    Ok(())
}

/// Capture a manifest only while its freshness witness still matches the
/// authoritative `nodes` row.
pub(crate) fn capture_chunked_content(
    conn: &Connection,
    node_id: &str,
) -> Result<Option<ChunkManifestSnapshot>> {
    if !chunk_schema_present(conn)? {
        return Ok(None);
    }

    let nodes_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for nodes table")?
        .unwrap_or(false);
    if !nodes_table {
        return Ok(None);
    }

    let witness: Option<(i64, Option<i64>, i64, i64)> = conn
        .query_row(
            "SELECT meta.source_len, meta.source_mtime, nodes.size, nodes.mtime
               FROM content_manifest_meta AS meta
               JOIN nodes ON nodes.id = meta.node_id
              WHERE meta.node_id = ?1",
            params![node_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .context("read chunk manifest freshness witness")?;
    let Some((source_len, source_mtime, live_len, live_mtime)) = witness else {
        return Ok(None);
    };
    if source_len < 0 || live_len < 0 || source_len != live_len || source_mtime != Some(live_mtime)
    {
        return Ok(None);
    }
    let source_len =
        usize::try_from(source_len).context("chunk manifest source length exceeds usize")?;

    let mut statement = conn
        .prepare(
            "SELECT chunk_hash, byte_offset, byte_len
               FROM content_manifest
              WHERE node_id = ?1
              ORDER BY seq",
        )
        .context("prepare chunk manifest snapshot")?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .context("read chunk manifest snapshot")?;

    let mut chunks = Vec::new();
    for row in rows {
        let (hash, offset, len) = row.context("decode chunk manifest row")?;
        anyhow::ensure!(
            hash.len() == blake3::OUT_LEN,
            "chunk manifest for {node_id} has a {}-byte hash",
            hash.len()
        );
        anyhow::ensure!(
            offset >= 0 && len >= 0,
            "chunk manifest for {node_id} has a negative span"
        );
        let hash: [u8; blake3::OUT_LEN] = hash
            .try_into()
            .map_err(|_| anyhow::anyhow!("validated BLAKE3 hash length changed"))?;
        chunks.push(leyline_cdc::Chunk {
            hash: leyline_core::Hash::from_bytes(hash),
            offset: usize::try_from(offset).context("chunk offset exceeds usize")?,
            len: usize::try_from(len).context("chunk length exceeds usize")?,
        });
    }
    if chunks.is_empty() {
        return Ok(None);
    }

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

    Ok(Some(ChunkManifestSnapshot { chunks, source_len }))
}

/// Refresh `node_id` after a known edit, but only if this arena already uses
/// chunk storage. A fresh previous manifest enables bounded incremental work;
/// otherwise the authoritative bytes are chunked in full.
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
            anyhow::ensure!(
                previous.source_len == old_len,
                "old manifest length {} does not match old record length {old_len}",
                previous.source_len
            );
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

/// Is chunk-backed content available AND provably fresh for `node_id`?
///
/// Three conditions, all required:
/// 1. the arena has chunk tables at all (an arena written by another runtime —
///    mache writes the `nodes` contract via `leyline-schema` — has none, and a
///    bare query would be a SQL error rather than "no");
/// 2. this node has manifest rows;
/// 3. the manifest's freshness witness still matches the node's CURRENT
///    `(size, mtime)`.
///
/// ## Why (3) exists — this is the load-bearing part
///
/// The manifest is a derived index over `nodes.record`, which is authoritative.
/// Every writer of `record` must invalidate the manifest, and enforcing that by
/// hand at each call site FAILED: an adversarial review found four writers
/// (`truncate`, `remove_node`, `rename_node`, `batch_splice`'s non-AST arm)
/// that left a live manifest behind. The worst was not staleness but
/// disclosure: node ids are PATHS and paths get reused, so an orphaned manifest
/// attaches to the next file created at that path — a brand-new, never-written
/// file served a deleted file's bytes.
///
/// A missed invalidation must therefore degrade to slow-but-correct, never to
/// silently-wrong. Comparing the witness against the live row does that: a
/// stale manifest is refused and the read falls back to `record`.
///
/// This covers writers OUTSIDE this crate too, which no amount of call-site
/// discipline here could. `leyline-ts`'s reproject deletes and re-inserts every
/// node with a fresh `mtime` (`ts/src/project.rs`), so its writes invalidate
/// implicitly. The explicit `invalidate_chunked_content*` calls are kept as
/// defense in depth and to stop orphaned rows accumulating — but correctness no
/// longer depends on remembering them.
///
/// A node with no `nodes` row is refused: there is nothing to prove freshness
/// against, and that is exactly the vacated-path case a rename leaves behind.
pub fn has_chunked_content(conn: &Connection, node_id: &str) -> Result<bool> {
    let tables_present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
               AND name IN ('content_manifest','content_manifest_meta','nodes')",
            [],
            |r| r.get(0),
        )
        .context("probe for chunk tables")?;
    if tables_present < 3 {
        return Ok(false);
    }

    // One query: manifest witness joined to the live node row. Any missing
    // side (no manifest, no node) yields no row, hence `false`.
    let fresh: Option<bool> = conn
        .query_row(
            "SELECT m.source_len = n.size AND m.source_mtime IS n.mtime \
               FROM content_manifest_meta m JOIN nodes n ON n.id = m.node_id \
              WHERE m.node_id = ?1",
            params![node_id],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )
        .optional()
        .context("check manifest freshness")?;
    let Some(true) = fresh else {
        return Ok(false);
    };

    // Witness matches; confirm the manifest actually has spans.
    let has_rows: bool = conn
        .query_row(
            "SELECT 1 FROM content_manifest WHERE node_id = ?1 LIMIT 1",
            params![node_id],
            |_| Ok(true),
        )
        .optional()
        .context("probe node manifest")?
        .unwrap_or(false);
    Ok(has_rows)
}

/// Which storage generation served a read.
///
/// Exposed because "the chunked path is working" and "the fallback is quietly
/// serving everything" are indistinguishable from the returned bytes alone —
/// both produce correct output. Without a marker, a migration that silently
/// failed to populate manifests would look exactly like success, just slower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentSource {
    /// Served from the chunk manifest — only overlapping chunks were read.
    Chunked,
    /// Served from `nodes.record` — the whole file was materialized to slice it.
    Record,
}

/// Process-wide tally of reads by source. Lets a long-running mount answer
/// "am I actually getting chunked reads?" without per-read logging noise.
static CHUNKED_READS: AtomicU64 = AtomicU64::new(0);
static RECORD_READS: AtomicU64 = AtomicU64::new(0);

/// `(chunked, record)` read counts since process start.
pub fn read_source_counts() -> (u64, u64) {
    (
        CHUNKED_READS.load(Ordering::Relaxed),
        RECORD_READS.load(Ordering::Relaxed),
    )
}

/// THE content read entry point — the one call site shape for serving a byte
/// range, whatever the arena's storage generation.
///
/// Chunk-backed when a manifest exists, otherwise the legacy `nodes.record`
/// path. The fallback is not a placeholder for unfinished work: `nodes` is a
/// cross-runtime contract that mache also writes, so arenas without chunk
/// tables are a permanent, valid input — not a migration state to be finished
/// and deleted. Both branches are pinned by tests that assert they return
/// identical bytes, so the fallback can never quietly become the only path
/// that works.
///
/// Use [`read_content_at_traced`] when the caller needs to know which path ran.
pub fn read_content_at(
    conn: &Connection,
    node_id: &str,
    buf: &mut [u8],
    offset: u64,
) -> Result<usize> {
    read_content_at_traced(conn, node_id, buf, offset).map(|(n, _)| n)
}

/// [`read_content_at`], additionally reporting which path served the read.
pub fn read_content_at_traced(
    conn: &Connection,
    node_id: &str,
    buf: &mut [u8],
    offset: u64,
) -> Result<(usize, ContentSource)> {
    if has_chunked_content(conn, node_id)? {
        CHUNKED_READS.fetch_add(1, Ordering::Relaxed);
        let n = read_content_chunked(conn, node_id, buf, offset)?;
        return Ok((n, ContentSource::Chunked));
    }
    RECORD_READS.fetch_add(1, Ordering::Relaxed);
    let n = read_content_from_record(conn, node_id, buf, offset)?;
    Ok((n, ContentSource::Record))
}

/// Legacy path: the whole file lives in `nodes.record`, so serving a range
/// means materializing all of it and slicing. Preserved verbatim for arenas
/// without chunk tables — and kept here, next to the chunked path, so the cost
/// difference between the two is impossible to miss when reading the code.
///
/// `pub(crate)` for symmetry with [`read_content_chunked`]: callers pick a
/// storage generation by accident if both raw readers are reachable. Go through
/// [`read_content_at`], which picks correctly and reports which ran.
pub(crate) fn read_content_from_record(
    conn: &Connection,
    node_id: &str,
    buf: &mut [u8],
    offset: u64,
) -> Result<usize> {
    let record: Option<String> = conn
        .query_row(
            "SELECT record FROM nodes WHERE id = ?1",
            params![node_id],
            |row| row.get(0),
        )
        .optional()
        .context("read node record")?
        .flatten();
    let Some(data) = record else {
        return Ok(0);
    };
    let bytes = data.as_bytes();
    let off = offset as usize;
    if off >= bytes.len() {
        return Ok(0);
    }
    let end = (off + buf.len()).min(bytes.len());
    let n = end - off;
    buf[..n].copy_from_slice(&bytes[off..end]);
    Ok(n)
}

/// Total byte length of `node_id`'s chunked content (manifest sum).
pub fn chunked_content_len(conn: &Connection, node_id: &str) -> Result<usize> {
    let n: Option<i64> = conn
        .query_row(
            "SELECT MAX(byte_offset + byte_len) FROM content_manifest WHERE node_id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .context("content length")?;
    Ok(n.unwrap_or(0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_cdc::{MAX_CHUNK, MIN_CHUNK};

    fn prng(seed: u64, n: usize) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        create_chunked_content_schema(&c).unwrap();
        c
    }

    /// Seeded xorshift — the fuzzer's only entropy source, so a failure is
    /// reproducible from the printed seed alone.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
    }

    /// Build a body with adversarial structure — CDC boundaries behave very
    /// differently on random bytes vs long constant runs vs repeated blocks
    /// (a repeated block should produce repeated *chunks*, exercising dedup
    /// and the `INSERT OR IGNORE` path).
    fn shaped(rng: &mut Rng, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            match rng.below(4) {
                0 => v.extend(std::iter::repeat_n(rng.next() as u8, rng.below(70_000) + 1)),
                1 => {
                    // Repeat an earlier region verbatim.
                    if v.is_empty() {
                        v.push(rng.next() as u8);
                    } else {
                        let a = rng.below(v.len());
                        let b = (a + rng.below(50_000) + 1).min(v.len());
                        let piece = v[a..b].to_vec();
                        v.extend_from_slice(&piece);
                    }
                }
                _ => v.extend((0..rng.below(60_000) + 1).map(|_| rng.next() as u8)),
            }
        }
        v.truncate(len);
        v
    }

    /// Independent oracle: read the whole manifest into Rust and count the
    /// truly-overlapping spans. Deliberately NOT SQL — it must be able to
    /// disagree with [`OVERLAP_PREDICATE`], or it proves nothing about it.
    fn expected_touched(conn: &Connection, node_id: &str, start: usize, len: usize) -> usize {
        let mut stmt = conn
            .prepare("SELECT byte_offset, byte_len FROM content_manifest WHERE node_id = ?1")
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map(params![node_id], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let end = start + len;
        spans
            .iter()
            .filter(|(off, l)| *off < end && off + l > start)
            .count()
    }

    #[test]
    fn full_read_round_trips() {
        let conn = db();
        let data = prng(1, 3_000_000);
        store_content_chunked(&conn, "n1", &data).unwrap();
        assert_eq!(chunked_content_len(&conn, "n1").unwrap(), data.len());

        let mut buf = vec![0u8; data.len()];
        let n = read_content_chunked(&conn, "n1", &mut buf, 0).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(buf, data);
    }

    #[test]
    fn range_reads_return_correct_bytes() {
        let conn = db();
        let data = prng(2, 3_000_000);
        store_content_chunked(&conn, "n1", &data).unwrap();

        for &(off, len) in &[
            (0usize, 100usize),
            (1_000_000, 250_000), // straddles many chunks
            (123_456, 4096),
            (data.len() - 10, 10),
        ] {
            let mut buf = vec![0u8; len];
            let n = read_content_chunked(&conn, "n1", &mut buf, off as u64).unwrap();
            assert_eq!(&buf[..n], &data[off..off + n], "range ({off},{len})");
            assert_eq!(n, len.min(data.len() - off));
        }
    }

    /// THE property: a small read of a large file makes the DB touch only the
    /// overlapping chunks — the whole file is never materialized. This is what
    /// `SELECT record` + slice cannot do.
    #[test]
    fn small_read_touches_only_overlapping_chunks() {
        let conn = db();
        let data = prng(3, 8_000_000);
        let total = store_content_chunked(&conn, "big", &data).unwrap();
        assert!(total > 50, "need a many-chunk file, got {total}");

        let mid = data.len() / 2;
        let touched = chunks_touched(&conn, "big", mid as u64, 4096).unwrap();
        assert!(
            touched <= 2,
            "a 4KiB read must touch <=2 of {total} chunks, touched {touched} — \
             the SQL layer must not re-materialize the file"
        );

        // ...and it still returns the right bytes.
        let mut buf = vec![0u8; 4096];
        let n = read_content_chunked(&conn, "big", &mut buf, mid as u64).unwrap();
        assert_eq!(&buf[..n], &data[mid..mid + n]);
    }

    /// The shipped overlap predicate selects EXACTLY the overlapping spans —
    /// no extras. Checked against a Rust-side oracle at boundary-aligned
    /// ranges, where a `<` → `<=` slip would silently pull in an adjacent
    /// zero-overlap chunk: correct bytes, wasted read. Correctness tests
    /// cannot see that; this one can.
    #[test]
    fn overlap_predicate_selects_exactly_the_overlapping_spans() {
        let conn = db();
        let data = prng(8, 4_000_000);
        store_content_chunked(&conn, "n", &data).unwrap();

        // Every chunk boundary, plus interior and degenerate offsets.
        let mut stmt = conn
            .prepare(
                "SELECT byte_offset, byte_len FROM content_manifest WHERE node_id='n' ORDER BY seq",
            )
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        let mut cases: Vec<(usize, usize)> = vec![(0, 1), (0, data.len()), (data.len() - 1, 1)];
        for (off, len) in &spans {
            cases.push((*off, *len)); // exactly one chunk
            cases.push((*off, 1)); // first byte of a chunk
            cases.push((off + len, 1)); // first byte of the NEXT chunk
            cases.push((off + len - 1, 2)); // straddles the boundary
        }

        for (off, len) in cases {
            if off + len > data.len() {
                continue;
            }
            let got = chunks_touched(&conn, "n", off as u64, len).unwrap();
            let want = expected_touched(&conn, "n", off, len);
            assert_eq!(got, want, "predicate over-/under-selects at ({off},{len})");
        }
    }

    /// Chunk-level dedup at the storage layer: two nodes sharing a large region
    /// store that region once — total chunk rows < total manifest spans.
    #[test]
    fn shared_content_is_stored_once() {
        let conn = db();
        let common = prng(4, 2_000_000);
        let mut a = prng(5, 200_000);
        let mut b = prng(6, 300_000);
        a.extend_from_slice(&common);
        b.extend_from_slice(&common);

        let na = store_content_chunked(&conn, "a", &a).unwrap();
        let nb = store_content_chunked(&conn, "b", &b).unwrap();

        let distinct: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_chunks", [], |r| r.get(0))
            .unwrap();
        assert!(
            (distinct as usize) < na + nb,
            "shared region must dedup: {distinct} distinct chunks vs {} spans",
            na + nb
        );
    }

    /// Re-storing a node replaces its manifest (no stale spans) and an edit
    /// re-uses the untouched chunks already in the store.
    #[test]
    fn restore_replaces_manifest_and_reuses_chunks() {
        let conn = db();
        let data = prng(7, 2_000_000);
        store_content_chunked(&conn, "n", &data).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_chunks", [], |r| r.get(0))
            .unwrap();

        let mut edited = data.clone();
        let mid = edited.len() / 2;
        edited.splice(mid..mid, [0xAA, 0xBB, 0xCC]);
        store_content_chunked(&conn, "n", &edited).unwrap();

        // Manifest reflects the new length exactly (old spans gone).
        assert_eq!(chunked_content_len(&conn, "n").unwrap(), edited.len());
        let mut buf = vec![0u8; edited.len()];
        read_content_chunked(&conn, "n", &mut buf, 0).unwrap();
        assert_eq!(buf, edited);

        // Boundary stability ⇒ the edit adds only a couple of chunks.
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_chunks", [], |r| r.get(0))
            .unwrap();
        assert!(
            after - before <= 3,
            "a 3-byte edit must add <=3 new chunks, added {}",
            after - before
        );
    }

    /// Differential fuzzer. For randomly shaped bodies and randomly chosen
    /// ranges, the chunked path must be indistinguishable from the naive
    /// `data[offset..offset+len]` it replaces — and must select exactly the
    /// overlapping chunks while doing it.
    ///
    /// Four invariants per case, each catching a different failure class:
    ///   1. **manifest tiles the file** — spans are contiguous, gapless, and
    ///      sum to `data.len()`. A dropped or overlapping span corrupts every
    ///      read that touches it.
    ///   2. **read == slice** — the whole point.
    ///   3. **selection is exact** — `chunks_touched` matches a Rust oracle,
    ///      so an over-selecting predicate (correct bytes, wasted I/O) is a
    ///      failure, not a silent regression.
    ///   4. **short read at EOF** — a range past the end returns what exists,
    ///      not a panic and not zero-padding.
    ///
    /// Deterministic: every case is derived from `SEED`, so a red run is
    /// reproducible from the assertion message alone.
    #[test]
    fn fuzz_chunked_reads_match_naive_slicing() {
        const SEED: u64 = 0x5DEE_CE66_D_u64;
        const CASES: usize = 120;
        let mut rng = Rng(SEED);

        for case in 0..CASES {
            let conn = db();
            // Mix of sub-chunk, single-chunk, and many-chunk files — the
            // MIN/MAX clamp edges are where span arithmetic goes wrong.
            let len = match case % 5 {
                0 => rng.below(64),            // smaller than MIN_CHUNK
                1 => rng.below(MIN_CHUNK * 2), // around the min clamp
                2 => rng.below(MAX_CHUNK * 2), // around the max clamp
                _ => rng.below(2_000_000),
            };
            let data = shaped(&mut rng, len);
            let node = format!("n{case}");
            store_content_chunked(&conn, &node, &data).unwrap();

            // (1) the manifest tiles [0, len) exactly.
            let mut stmt = conn
                .prepare(
                    "SELECT byte_offset, byte_len FROM content_manifest                       WHERE node_id = ?1 ORDER BY seq",
                )
                .unwrap();
            let spans: Vec<(usize, usize)> = stmt
                .query_map(params![&node], |r| {
                    Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let mut cursor = 0usize;
            for (off, l) in &spans {
                assert_eq!(
                    *off, cursor,
                    "case {case} (seed {SEED:#x}): manifest gap/overlap"
                );
                cursor += l;
            }
            assert_eq!(
                cursor,
                data.len(),
                "case {case} (seed {SEED:#x}): manifest does not cover the file"
            );
            assert_eq!(chunked_content_len(&conn, &node).unwrap(), data.len());

            // (2)(3)(4) probe ranges, including boundary-aligned and past-EOF.
            for probe in 0..12 {
                let (off, want_len) = match probe {
                    0 => (0, data.len()),
                    1 if !spans.is_empty() => {
                        let (o, l) = spans[rng.below(spans.len())];
                        (o, l) // exactly one chunk
                    }
                    2 if !spans.is_empty() => {
                        let (o, l) = spans[rng.below(spans.len())];
                        (o + l, 1) // first byte after a boundary
                    }
                    3 => (data.len(), 16),                   // wholly past EOF
                    4 => (data.len().saturating_sub(3), 64), // straddles EOF
                    _ => {
                        let o = rng.below(data.len() + 1);
                        (o, rng.below(200_000) + 1)
                    }
                };
                if want_len == 0 {
                    continue;
                }

                let mut buf = vec![0xEEu8; want_len];
                let n = read_content_chunked(&conn, &node, &mut buf, off as u64).unwrap();

                let expect = &data[off.min(data.len())..(off + want_len).min(data.len())];
                assert_eq!(
                    n,
                    expect.len(),
                    "case {case} probe {probe} (seed {SEED:#x}): short-read length at ({off},{want_len})"
                );
                assert_eq!(
                    &buf[..n],
                    expect,
                    "case {case} probe {probe} (seed {SEED:#x}): bytes differ at ({off},{want_len})"
                );

                let got = chunks_touched(&conn, &node, off as u64, want_len).unwrap();
                let want = expected_touched(&conn, &node, off, want_len);
                assert_eq!(
                    got, want,
                    "case {case} probe {probe} (seed {SEED:#x}): selection is not exact at ({off},{want_len})"
                );
            }
        }
    }

    /// Does the range read actually USE the span index, or does SQLite scan the
    /// whole manifest? The module's entire claim rests on the answer, so it is
    /// pinned here rather than assumed.
    #[test]
    fn range_read_uses_the_span_index() {
        let conn = db();
        let data = prng(9, 4_000_000);
        store_content_chunked(&conn, "n", &data).unwrap();
        conn.execute_batch("ANALYZE").unwrap();

        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT byte_offset, byte_len, chunk_bytes \
               FROM content_manifest \
               JOIN content_chunks USING (chunk_hash) \
              WHERE {OVERLAP_PREDICATE}"
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(
                params!["n", 100_000i64, 90_000i64, seek_floor(90_000)],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let joined = plan.join(" | ");
        eprintln!("PLAN: {joined}");
        assert!(
            joined.contains("content_manifest_span"),
            "range read must use content_manifest_span; plan was: {joined}"
        );
        assert!(
            !joined.contains("SCAN content_manifest"),
            "range read must not full-scan the manifest; plan was: {joined}"
        );
        // BOTH bounds must be index bounds, not post-filters. With only the
        // upper bound, a read near the end of a large file walks every index
        // entry from offset 0 forward — correct bytes, O(file) index work.
        // Dropping the `?4` seek floor regresses to exactly that, and this
        // assertion is the only thing that would notice: every correctness
        // test still passes without it.
        assert!(
            joined.contains("byte_offset>?") && joined.contains("byte_offset<?"),
            "both range bounds must be driven by the index, not post-filtered; \
             plan was: {joined}"
        );
    }

    /// The seek floor must be both SOUND (never above the first overlapping
    /// chunk, or reads lose data) and TIGHT (close to `start`, or the index
    /// seek degenerates to walking from offset 0 — correct but O(file)).
    ///
    /// Soundness alone is not enough: `seek_floor(_) = 0` is perfectly sound
    /// and defeats the entire optimization. Mutation testing surfaced exactly
    /// that — `replace seek_floor -> i64 with 0` survived every other test,
    /// including the EXPLAIN QUERY PLAN one, because the plan still shows an
    /// index bound when that bound is the useless value 0.
    #[test]
    fn seek_floor_is_sound_and_tight() {
        let conn = db();
        let data = prng(11, 6_000_000);
        store_content_chunked(&conn, "n", &data).unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT byte_offset, byte_len FROM content_manifest \
                  WHERE node_id = 'n' ORDER BY seq",
            )
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for start in [0, 1, MAX_CHUNK, 1_000_000, 3_000_000, data.len() - 1] {
            let floor = seek_floor(start) as usize;

            // SOUND: no chunk overlapping `start` may begin below the floor.
            let first_overlapping = spans
                .iter()
                .find(|(off, l)| off + l > start)
                .expect("some chunk covers every offset");
            assert!(
                first_overlapping.0 >= floor,
                "floor {floor} excludes overlapping chunk at {} (start {start})",
                first_overlapping.0
            );

            // TIGHT: within one MAX_CHUNK of the request. This is what makes
            // the index seek bounded instead of a walk from the beginning.
            assert!(
                start - floor <= MAX_CHUNK,
                "floor {floor} is {} below start {start} — seek is unbounded",
                start - floor
            );
        }

        // And for a deep read the floor must actually be off the floor.
        assert!(
            seek_floor(3_000_000) > 0,
            "a read 3MB into a file must not seek from offset 0"
        );
    }

    /// Minimal `nodes` row so the record-fallback path has something to read.
    fn insert_node_record(conn: &Connection, id: &str, content: &str) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nodes (
                 id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL,
                 kind INTEGER NOT NULL, size INTEGER DEFAULT 0,
                 mtime INTEGER NOT NULL, record_id TEXT, record JSON,
                 source_file TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
             VALUES (?1, '', ?1, 0, ?2, 1, ?3)",
            params![id, content.len() as i64, content],
        )
        .unwrap();
    }

    /// The two storage generations must be observationally identical: for the
    /// same content and the same range, chunked and record return the same
    /// bytes. If they ever diverge, one arena generation silently serves
    /// different data than the other for the same logical file.
    #[test]
    fn chunked_and_record_paths_agree_byte_for_byte() {
        let conn = db();
        // Text, because the record column is TEXT — the fallback reads a String.
        let content: String = (0..40_000)
            .map(|i| ((i * 7 % 26) as u8 + b'a') as char)
            .collect();

        insert_node_record(&conn, "legacy", &content); // record only
        insert_node_record(&conn, "modern", &content); // record AND manifest
        store_content_chunked(&conn, "modern", content.as_bytes()).unwrap();

        for &(off, len) in &[
            (0usize, 100usize),
            (12_345, 9_000),
            (39_990, 50),
            (0, 40_000),
        ] {
            let mut a = vec![0u8; len];
            let (na, sa) = read_content_at_traced(&conn, "legacy", &mut a, off as u64).unwrap();
            let mut b = vec![0u8; len];
            let (nb, sb) = read_content_at_traced(&conn, "modern", &mut b, off as u64).unwrap();

            // The marker must show the paths genuinely differed...
            assert_eq!(
                sa,
                ContentSource::Record,
                "legacy node must use the record path"
            );
            assert_eq!(
                sb,
                ContentSource::Chunked,
                "modern node must use the chunk path"
            );
            // ...and the bytes must be identical anyway.
            assert_eq!(na, nb, "length differs at ({off},{len})");
            assert_eq!(a[..na], b[..nb], "bytes differ at ({off},{len})");
            assert_eq!(&a[..na], &content.as_bytes()[off..off + na]);
        }
    }

    /// An arena from another runtime has NO chunk tables at all. Probing must
    /// report "no manifest", not raise a SQL error — mache writes the `nodes`
    /// contract without ever creating `content_manifest`.
    #[test]
    fn foreign_arena_without_chunk_tables_falls_back_cleanly() {
        let conn = Connection::open_in_memory().unwrap(); // NOTE: no chunk schema
        insert_node_record(&conn, "n", "hello world");

        assert!(!has_chunked_content(&conn, "n").unwrap());

        let mut buf = vec![0u8; 5];
        let (n, src) = read_content_at_traced(&conn, "n", &mut buf, 6).unwrap();
        assert_eq!(&buf[..n], b"world");
        assert_eq!(src, ContentSource::Record);
    }

    /// The counters must actually move, or they cannot answer "is the mount
    /// really getting chunked reads?" — the question they exist for.
    #[test]
    fn read_source_counters_track_both_paths() {
        let conn = db();
        insert_node_record(&conn, "r", "abcdefghij");
        insert_node_record(&conn, "c", "abcdefghij");
        store_content_chunked(&conn, "c", b"abcdefghij").unwrap();

        let (c0, r0) = read_source_counts();
        let mut buf = vec![0u8; 4];
        read_content_at(&conn, "c", &mut buf, 0).unwrap();
        read_content_at(&conn, "r", &mut buf, 0).unwrap();
        let (c1, r1) = read_source_counts();

        // `>=`, not `==`: these are process-global counters and the test
        // harness runs tests in parallel, so a sibling test's reads land in
        // the same tally. An exact assertion here would be a flake, not a
        // stronger check — and the bug this guards ("the counter never moves")
        // is caught either way. Which branch increments which counter is
        // pinned exactly by the ContentSource assertions above.
        assert!(c1 - c0 >= 1, "a chunked read should have been counted");
        assert!(r1 - r0 >= 1, "a record read should have been counted");
    }

    /// The structural guarantee, stated as a test: a writer this crate knows
    /// NOTHING about mutates `nodes.record` and never touches the manifest.
    /// The read must refuse the stale manifest and fall back to `record`.
    ///
    /// This is what makes correctness independent of call-site discipline.
    /// Hand-enforced invalidation demonstrably failed — an adversarial review
    /// found four writers in `graph.rs` that skipped it, one of which leaked a
    /// deleted file's bytes into a newly created file at the same path.
    /// Verified: strip every explicit `invalidate_chunked_content*` call from
    /// `graph.rs` and the whole suite still passes; additionally neuter this
    /// freshness check and the bugs return.
    #[test]
    fn an_unknown_writer_cannot_cause_stale_bytes_to_be_served() {
        let conn = db();
        insert_node_record(&conn, "n", "original-content");
        store_content_chunked(&conn, "n", b"original-content").unwrap();

        let mut buf = vec![0u8; 64];
        let (n, src) = read_content_at_traced(&conn, "n", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"original-content");
        assert_eq!(
            src,
            ContentSource::Chunked,
            "precondition: served from chunks"
        );

        // A writer with no knowledge of chunk tables replaces the content.
        // Same shape as leyline-ts's reproject: update `record`, bump mtime.
        conn.execute(
            "UPDATE nodes SET record = ?1, size = ?2, mtime = mtime + 1 WHERE id = 'n'",
            params!["REPLACED-by-a-stranger", 21i64],
        )
        .unwrap();

        let mut buf = vec![0u8; 64];
        let (n, src) = read_content_at_traced(&conn, "n", &mut buf, 0).unwrap();
        assert_eq!(
            &buf[..n],
            b"REPLACED-by-a-stranger",
            "served stale chunked bytes after an unknown writer changed `record`"
        );
        assert_eq!(
            src,
            ContentSource::Record,
            "must degrade to the record path, not keep trusting the manifest"
        );
    }

    /// Freshness must also catch a same-LENGTH edit, where only `mtime` moves.
    /// A length-only witness would pass this and serve the wrong bytes.
    #[test]
    fn freshness_catches_an_equal_length_rewrite() {
        let conn = db();
        insert_node_record(&conn, "n", "aaaaaaaa");
        store_content_chunked(&conn, "n", b"aaaaaaaa").unwrap();

        conn.execute(
            "UPDATE nodes SET record = 'bbbbbbbb', mtime = mtime + 1 WHERE id = 'n'",
            [],
        )
        .unwrap();

        let mut buf = vec![0u8; 32];
        let (n, src) = read_content_at_traced(&conn, "n", &mut buf, 0).unwrap();
        assert_eq!(
            &buf[..n],
            b"bbbbbbbb",
            "equal-length rewrite served stale bytes"
        );
        assert_eq!(src, ContentSource::Record);
    }

    fn manifest_rows(conn: &Connection, node_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM content_manifest WHERE node_id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Invalidation's remaining job is HYGIENE, not correctness.
    ///
    /// Since the freshness witness landed, making these functions no-ops does
    /// not break any read — mutation testing confirmed it by leaving
    /// `invalidate_chunked_content -> Ok(())` alive against the whole suite.
    /// That is the structural fix working as intended, but it also means
    /// nothing was pinning what these functions still owe: actually deleting
    /// the rows, so orphaned manifests don't accumulate forever in an arena
    /// that is supposed to stay a single portable `.db`.
    #[test]
    fn invalidation_actually_deletes_the_manifest_rows() {
        let conn = db();
        insert_node_record(&conn, "n", "some content here");
        store_content_chunked(&conn, "n", b"some content here").unwrap();
        assert!(manifest_rows(&conn, "n") > 0, "precondition: rows exist");

        invalidate_chunked_content(&conn, "n").unwrap();
        assert_eq!(
            manifest_rows(&conn, "n"),
            0,
            "invalidation left orphaned manifest rows behind"
        );
    }

    /// The subtree variant must cascade exactly like the `id LIKE 'x/%'`
    /// deletes it mirrors — a child's manifest outliving its parent is the
    /// orphan case that grows unboundedly.
    #[test]
    fn subtree_invalidation_cascades_to_descendants() {
        let conn = db();
        for id in ["docs", "docs/readme", "docs/deep/nested", "docsibling"] {
            insert_node_record(&conn, id, "content");
            store_content_chunked(&conn, id, b"content").unwrap();
        }

        invalidate_chunked_content_subtree(&conn, "docs").unwrap();

        assert_eq!(manifest_rows(&conn, "docs"), 0, "self not invalidated");
        assert_eq!(
            manifest_rows(&conn, "docs/readme"),
            0,
            "child not invalidated"
        );
        assert_eq!(
            manifest_rows(&conn, "docs/deep/nested"),
            0,
            "grandchild not invalidated"
        );
        // Prefix-sibling must NOT be caught: "docs" must not match "docsibling".
        assert!(
            manifest_rows(&conn, "docsibling") > 0,
            "cascade over-matched a prefix sibling"
        );
    }
}
