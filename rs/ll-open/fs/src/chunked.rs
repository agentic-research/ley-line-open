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
//! bounded resync window and stores only its new chunks — the manifest store
//! is told the rescan window and verifies carried-over rows by existence
//! probe, never by re-hashing (this was false until ley-line-open-f8ebe7:
//! the store re-hashed every chunk, making refresh O(file) while the stat
//! that should have caught it measured the scanner one layer down). Initial
//! population, a missing manifest, or a stale freshness witness deliberately
//! falls back to a full chunk.
//!
//! ## Public surface — one way in
//!
//! [`read_content_at`] / [`read_content_at_traced`] are the ONLY public readers.
//! The raw readers are `pub(crate)` on purpose: they bypass the freshness gate
//! in [`has_chunked_content`], and that gate is what keeps a missed
//! invalidation from becoming silent data corruption. Making the unsafe path
//! unreachable is stronger than documenting that it exists.
//!
//! ## Canonical operation ownership
//!
//! Each operation has one owner. SQL in this module selects ordered
//! `(hash, offset, len)` addresses; [`SqliteBlobStore`] owns physical blob
//! access and verify-on-read; [`leyline_cdc::read_range_into`] owns span
//! validation and reconstruction. A SQL JOIN that also returns `chunk_bytes`,
//! or a second copy loop in this module, duplicates an owned operation and is
//! an architecture violation rather than an alternative implementation.
//!
//! ## What this does NOT do (stated so the claims stay honest)
//!
//! - **Freshness, selection, and blob reads share one snapshot.** The public
//!   read path owns one SQLite read transaction, selects only overlapping
//!   manifest rows, and fetches them through [`SqliteBlobStore`].
//!   `BlobStore::get` verifies every selected blob before the caller's output
//!   buffer is changed; integrity errors fail closed and never retry through
//!   `nodes.record`.
//! - **Garbage collection is explicit.** Invalidating a manifest deliberately
//!   leaves chunk bytes available for immediate reuse. Long-lived projections
//!   bound that history with [`crate::gc::collect_unreachable_chunks`], an
//!   off-hot-path transactional sweep that first reaps manifests whose
//!   freshness witness cannot be satisfied — a manifest every read already
//!   refuses still *pins* its chunks, so without that step the collector
//!   reports success and reclaims nothing (bead `ley-line-open-b5e56f`) —
//!   and then deletes the chunks no surviving manifest references.
//! - **Authority is scoped.** These tables are a DERIVED accelerator for this
//!   crate's read path, not an identity domain. They have no root, are not a
//!   cross-process contract, and are never the canonical substrate;
//!   `nodes.record` holds that role. See ADR-0032 §D4 for the authority
//!   assignment and ADR-0033 D1 for why the dual store is permanent rather
//!   than a migration state.
//! - **The manifest is a derived index, not the source of truth.**
//!   `nodes.record` remains authoritative — it is the cross-runtime contract
//!   (`leyline-schema`: "mache writes it, leyline-fs reads it"). The manifest
//!   accelerates reads and MUST be invalidated whenever `record` changes
//!   behind this crate's back. `leyline-ts`'s splice/reproject does exactly
//!   that, so `graph.rs` invalidates after both `flush_node` and
//!   `batch_splice`. A writer outside this crate that updates `record` while
//!   leaving a manifest in place would serve stale bytes — that is the
//!   invariant to preserve when adding write paths.

use anyhow::{Context, Result, ensure};
use leyline_core::{BlobStore, ContentAddressed, Hash};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::atomic::{AtomicU64, Ordering};

/// The shared content-addressed chunk pool. Mirrors the `source_blobs` shape
/// so everything durable stays inside the one `.db`. Split from the per-node
/// manifest DDL because BOTH activation targets store into it — `nodes`
/// manifests (this module) and `source_blobs` manifests
/// ([`crate::blob_chunked`]) address the same rows, which is what makes
/// identical content reached through either target cost one row.
pub(crate) const CHUNK_POOL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS content_chunks (
    chunk_hash  BLOB PRIMARY KEY,
    chunk_bytes BLOB NOT NULL,
    -- Named chunk_len, not byte_len: the manifest's byte_len joins against
    -- this table, and a shared name makes the join predicate ambiguous.
    chunk_len   INTEGER GENERATED ALWAYS AS (length(chunk_bytes)) STORED
);";

/// Per-node manifest plus its freshness-witness apparatus. This half is
/// specific to the MUTABLE `nodes` target; the blob target deliberately has
/// none of it (see `blob_chunked` — existence is freshness there).
const NODE_MANIFEST_DDL: &str = "\
CREATE TABLE IF NOT EXISTS content_manifest (
    nid        INTEGER NOT NULL,
    seq        INTEGER NOT NULL,
    chunk_hash BLOB    NOT NULL,
    byte_offset INTEGER NOT NULL,
    byte_len    INTEGER NOT NULL,
    PRIMARY KEY (nid, seq)
);

-- The index that makes a range read a WHERE clause rather than a full scan.
CREATE INDEX IF NOT EXISTS content_manifest_span
    ON content_manifest(nid, byte_offset);

-- The index that makes reachability GC one lookup per distinct chunk instead
-- of a full manifest scan per chunk.
CREATE INDEX IF NOT EXISTS content_manifest_chunk_hash
    ON content_manifest(chunk_hash);

-- Freshness witness: the generation and length of the `nodes` row this
-- manifest was built from. A read compares them against the row's CURRENT
-- values, so a manifest whose source moved on is REFUSED rather than served.
-- This is what makes a missed invalidation degrade to slow-but-correct
-- instead of silently wrong. See `has_chunked_content`.
--
-- `source_generation` is the load-bearing half (ley-line-open-b82f56): the
-- old (size, mtime) pair was a change HEURISTIC, not a mutation identity — a
-- same-length replacement within mtime granularity, or any writer reproducing
-- both, served the previous occupant's bytes. The generation is bumped by a
-- TRIGGER on `nodes` (see GENERATION_TRIGGERS_DDL), so every writer that goes
-- through SQL advances it — including foreign writers that have never heard
-- of this module, which is exactly the writer class the witness exists for.
-- `source_mtime` remains as a column for older readers but no longer enters
-- the freshness predicate.
CREATE TABLE IF NOT EXISTS content_manifest_meta (
    nid               INTEGER PRIMARY KEY,
    source_len        INTEGER NOT NULL,
    source_mtime      INTEGER,
    source_generation INTEGER NOT NULL DEFAULT -1
);

-- One row per node: how many times `nodes.record` has been replaced (or the
-- row re-inserted) since this arena gained the CDC schema. Maintained by
-- triggers, never by application code.
CREATE TABLE IF NOT EXISTS content_generation (
    nid        INTEGER PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 0
);";

/// Triggers that make `content_generation` writer-proof. Separate from the
/// base DDL because they reference `nodes`, which pure content-addressed
/// arenas (some tests, non-graph use) do not have; applied wherever the
/// witness is actually consulted — see [`ensure_generation_infra`].
///
/// INSERT bumps too: a fresh row at a reused nid is the disclosure scenario
/// and it must invalidate any manifest the old occupant left. projection-v5
/// narrows that scenario but does not remove it — `files` is append-only, so
/// a nid range can never re-bind to an unrelated path, but deleting a file
/// and re-creating it AT THE SAME PATH re-binds to the same `file_id` and
/// therefore the same nid. The generation bump on the new row's INSERT is
/// what makes the old occupant's manifest read stale.
const GENERATION_TRIGGERS_DDL: &str = "\
CREATE TRIGGER IF NOT EXISTS content_generation_on_update
AFTER UPDATE OF record ON nodes
WHEN new.record IS NOT old.record
BEGIN
    INSERT INTO content_generation(nid, generation) VALUES (new.nid, 1)
    ON CONFLICT(nid) DO UPDATE SET generation = generation + 1;
END;

CREATE TRIGGER IF NOT EXISTS content_generation_on_insert
AFTER INSERT ON nodes
BEGIN
    INSERT INTO content_generation(nid, generation) VALUES (new.nid, 1)
    ON CONFLICT(nid) DO UPDATE SET generation = generation + 1;
END;";

/// The freshness witness, defined ONCE (types-friend F3 — the module already
/// states the rule for `OVERLAP_PREDICATE`: a second hand-written copy is how
/// the read gate and the GC reaper drift apart, and they had). `m` is the
/// witness table alias, `n` the nodes alias. The Rust arm is
/// [`manifest_witness_is_fresh`]; `witness_predicate_arms_agree` drives both
/// over one truth table.
///
/// `COALESCE(n.size, -1)`: a NULL `size` (the column is nullable in the
/// canonical DDL) must read as NOT FRESH, not as a SQL NULL that errors at
/// decode (types-friend F4).
pub(crate) const WITNESS_FRESH_PREDICATE: &str = "\
m.source_len >= 0 \
AND m.source_len = COALESCE(n.size, -1) \
AND m.source_generation = COALESCE(\
    (SELECT g.generation FROM content_generation g WHERE g.nid = n.nid), 0)";

/// Create the chunk store + manifest tables (idempotent), migrate a
/// pre-generation witness table, and install the generation triggers when
/// the arena has a `nodes` table to hang them on.
pub fn create_chunked_content_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CHUNK_POOL_DDL)
        .context("create shared chunk pool")?;
    conn.execute_batch(NODE_MANIFEST_DDL)
        .context("create chunked content schema")?;
    ensure_generation_infra(conn)
}

/// Idempotently bring an arena's generation infrastructure current: create
/// `content_generation` when absent, and install the `nodes` triggers when
/// `nodes` exists.
///
/// Called from schema creation, the manifest store path, and GC — any of
/// which may be the first writer to touch an arena that has the chunk pool
/// but not the witness. All statements are IF-NOT-EXISTS-shaped, and SQLite
/// DDL is transactional, so a dry-run GC that runs this inside its
/// rolled-back transaction stays non-mutating.
///
/// The pre-b82f56 `ALTER TABLE ... ADD COLUMN source_generation` shim is
/// gone: projection-v5 re-keys every one of these tables on `nid`, so a
/// pre-v5 arena is not upgradable in place and is not a supported input.
/// The projection is derived — it is re-parsed, not migrated.
pub(crate) fn ensure_generation_infra(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_generation (
             nid        INTEGER PRIMARY KEY,
             generation INTEGER NOT NULL DEFAULT 0
         );",
    )
    .context("create content_generation")?;

    let nodes_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for nodes table")?
        .unwrap_or(false);
    if nodes_table {
        conn.execute_batch(GENERATION_TRIGGERS_DDL)
            .context("install generation triggers")?;
    }
    Ok(())
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

pub(crate) struct SqliteBlobStore<'tx, 'conn> {
    tx: &'tx Transaction<'conn>,
}

impl<'tx, 'conn> SqliteBlobStore<'tx, 'conn> {
    /// Crate-wide constructor: `blob_chunked` stores into and reads from the
    /// SAME pool as this module, through the same verify-on-read gate.
    pub(crate) fn new(tx: &'tx Transaction<'conn>) -> Self {
        Self { tx }
    }
}

impl BlobStore for SqliteBlobStore<'_, '_> {
    fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
        let hash = bytes.hash();
        let inserted = self
            .tx
            .execute(
                "INSERT OR IGNORE INTO content_chunks (chunk_hash, chunk_bytes) VALUES (?1, ?2)",
                params![hash.as_bytes().as_slice(), bytes],
            )
            .context("insert SQLite blob")?;
        if inserted == 0 {
            ensure!(
                self.get(hash)?.is_some(),
                "SQLite blob disappeared after occupied insert"
            );
        }
        Ok(hash)
    }

    fn get(&self, hash: Hash) -> Result<Option<Vec<u8>>> {
        let bytes = self
            .tx
            .query_row(
                "SELECT chunk_bytes FROM content_chunks WHERE chunk_hash = ?1",
                params![hash.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .context("read SQLite blob")?;

        let Some(bytes) = bytes else {
            return Ok(None);
        };
        ensure!(
            bytes.as_slice().hash() == hash,
            "SqliteBlobStore integrity violation"
        );
        Ok(Some(bytes))
    }

    fn contains(&self, hash: Hash) -> Result<bool> {
        self.tx
            .query_row(
                "SELECT 1 FROM content_chunks WHERE chunk_hash = ?1",
                params![hash.as_bytes().as_slice()],
                |_| Ok(true),
            )
            .optional()
            .context("probe SQLite blob")
            .map(|present| present.unwrap_or(false))
    }
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

/// Store `data` for `nid` as content-defined chunks, replacing any existing
/// manifest. Chunk bytes are `INSERT OR IGNORE`d, so a chunk shared with another
/// file (or an earlier version of this one) costs nothing. Returns the chunk
/// count.
pub fn store_content_chunked(conn: &Connection, nid: i64, data: &[u8]) -> Result<usize> {
    let chunks = leyline_cdc::chunk(data);
    let all = 0..chunks.len();
    store_content_manifest(conn, nid, data, &chunks, all)
}

fn store_content_manifest(
    conn: &Connection,
    nid: i64,
    data: &[u8],
    chunks: &[leyline_cdc::Chunk],
    rehashed: std::ops::Range<usize>,
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
    store_content_manifest_in_transaction(&tx, nid, data, chunks, rehashed)?;
    tx.commit().context("commit chunked store")?;
    Ok(chunks.len())
}

/// Store authoritative bytes inside a transaction owned by the caller.
///
/// This is crate-private so activation can acquire an IMMEDIATE transaction,
/// read `nodes.record`, and write its manifest without a time-of-check/time-of-
/// use gap. The transaction is not committed here.
pub(crate) fn store_content_chunked_in_transaction(
    tx: &Transaction<'_>,
    nid: i64,
    data: &[u8],
) -> Result<usize> {
    let chunks = leyline_cdc::chunk(data);
    let all = 0..chunks.len();
    store_content_manifest_in_transaction(tx, nid, data, &chunks, all)?;
    Ok(chunks.len())
}

/// `rehashed` is the index range of chunks whose bytes are NEW under this
/// write — the incremental path's rescan window, or `0..chunks.len()` for a
/// full chunking. Only those chunks are hashed and `put`; rows outside the
/// range were carried over from a manifest that already stored their blobs,
/// so they are verified by existence (`BlobStore::contains`, an index probe)
/// rather than re-hashed. This is what makes a small edit cost
/// O(edit + resync window) in hashing instead of O(file) — the sub-file
/// premise the whole CDC layer exists for (ley-line-open-f8ebe7).
///
/// THE TRADE, NAMED (types-friend F2/F6): the old whole-file re-hash was
/// incidentally the enforcement that the caller's [`leyline_cdc::Edit`]
/// coordinates were truthful. With it gone, that obligation is carried by:
/// the [`leyline_cdc::Edit`] parse type (ordering unrepresentable), the
/// graph write path deriving coordinates from the write op itself and
/// falling back to a full re-chunk when its UTF-8-lossy conversion changed
/// the bytes, and the end-to-end oracle test
/// (`graph_write_incrementally_matches_full_chunk_oracle`) asserting the
/// stored manifest equals `chunk(new_data)`. Do not add a carried-row
/// shortcut for a NEW caller without walking that chain.
fn store_content_manifest_in_transaction(
    tx: &Transaction<'_>,
    nid: i64,
    data: &[u8],
    chunks: &[leyline_cdc::Chunk],
    rehashed: std::ops::Range<usize>,
) -> Result<()> {
    ensure!(
        rehashed.start <= rehashed.end && rehashed.end <= chunks.len(),
        "rehashed window {rehashed:?} does not lie within the {} manifest rows",
        chunks.len()
    );
    tx.execute("DELETE FROM content_manifest WHERE nid = ?1", params![nid])
        .context("clear previous manifest")?;

    let mut put_span = tx
        .prepare(
            "INSERT INTO content_manifest (nid, seq, chunk_hash, byte_offset, byte_len) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .context("prepare manifest insert")?;
    let mut store = SqliteBlobStore { tx };

    for (seq, c) in chunks.iter().enumerate() {
        if rehashed.contains(&seq) {
            let bytes = &data[c.offset..c.offset + c.len];
            let stored = store.put(bytes).context("insert chunk")?;
            ensure!(stored == c.hash, "chunk manifest hash does not match bytes");
        } else {
            // Carried over from the previous manifest: the blob was stored
            // (and hash-verified) when it was first written, and blobs are
            // immutable under their content address. Existence is the whole
            // check — catches a GC that reaped a still-referenced chunk.
            ensure!(
                store.contains(c.hash).context("probe carried chunk")?,
                "carried-over chunk {seq} is missing from the blob store"
            );
        }
        put_span
            .execute(params![
                nid,
                i64::try_from(seq).context("chunk sequence exceeds SQLite INTEGER")?,
                c.hash.as_bytes().as_slice(),
                i64::try_from(c.offset).context("chunk offset exceeds SQLite INTEGER")?,
                i64::try_from(c.len).context("chunk length exceeds SQLite INTEGER")?
            ])
            .context("insert manifest span")?;
    }
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
    // Option decodes throughout (types-friend F4): `record` and `size` are
    // nullable in the canonical `nodes` DDL, and a NULL must be a NAMED
    // refusal here, not a rusqlite InvalidColumnType error.
    let node_meta: Option<(Option<Vec<u8>>, Option<i64>, i64)> = if nodes_table {
        ensure_generation_infra(tx).context("ensure generation infra before store")?;
        tx.query_row(
            "SELECT CAST(record AS BLOB), size, mtime FROM nodes WHERE nid = ?1",
            params![nid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .context("read node freshness witness")?
    } else {
        None
    };
    if let Some((record, source_len, _)) = &node_meta {
        let (Some(record), Some(source_len)) = (record, source_len) else {
            anyhow::bail!(
                "cannot chunk-store nid {nid}: its nodes row has a NULL record \
                 or size, so no freshness witness can be captured"
            );
        };
        ensure!(
            *source_len >= 0
                && usize::try_from(*source_len).ok() == Some(data.len())
                && record == data,
            "authoritative node changed before chunk store for nid {nid}"
        );
    }
    // The generation captured in the SAME transaction as the manifest —
    // the mutation identity this witness is keyed on (b82f56). Zero when
    // the node predates the triggers or there is no nodes row.
    let generation: i64 = tx
        .query_row(
            "SELECT COALESCE(\
                 (SELECT generation FROM content_generation WHERE nid = ?1), 0)",
            params![nid],
            |r| r.get(0),
        )
        .context("read content generation")?;
    tx.execute(
        "INSERT OR REPLACE INTO content_manifest_meta \
         (nid, source_len, source_mtime, source_generation) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            nid,
            i64::try_from(data.len()).context("source length exceeds SQLite INTEGER")?,
            node_meta.map(|(_, _, mtime)| mtime),
            generation
        ],
    )
    .context("record manifest freshness witness")?;
    Ok(())
}

/// The overlap predicate, defined once. `?1` = nid, `?2` = range end,
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
const OVERLAP_PREDICATE: &str = "nid = ?1 AND byte_offset >= ?4 AND byte_offset < ?2 \
     AND (byte_offset + byte_len) > ?3";

fn select_range_manifest_sql() -> String {
    format!(
        "SELECT chunk_hash, byte_offset, byte_len \
           FROM content_manifest \
          WHERE {OVERLAP_PREDICATE} \
          ORDER BY byte_offset, seq"
    )
}

pub(crate) fn decode_manifest_chunk(
    owner: &dyn std::fmt::Display,
    hash: Vec<u8>,
    offset: i64,
    len: i64,
) -> Result<leyline_cdc::Chunk> {
    ensure!(
        hash.len() == blake3::OUT_LEN,
        "chunk manifest for {owner} has a {}-byte hash",
        hash.len()
    );
    let hash: [u8; blake3::OUT_LEN] = hash
        .try_into()
        .map_err(|_| anyhow::anyhow!("validated BLAKE3 hash length changed"))?;
    Ok(leyline_cdc::Chunk {
        hash: Hash::from_bytes(hash),
        offset: usize::try_from(offset)
            .context("chunk manifest offset is negative or too large")?,
        len: usize::try_from(len).context("chunk manifest length is negative or too large")?,
    })
}

pub(crate) fn sqlite_integer(value: usize, label: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{label} exceeds SQLite INTEGER"))
}

fn select_range_manifest(
    tx: &Transaction<'_>,
    nid: i64,
    offset: usize,
    len: usize,
) -> Result<Option<leyline_cdc::SelectedRange>> {
    let source_len = tx
        .query_row(
            "SELECT source_len FROM content_manifest_meta WHERE nid = ?1",
            params![nid],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("read range manifest source length")?;
    let Some(source_len) = source_len else {
        return Ok(None);
    };
    let source_len = usize::try_from(source_len)
        .context("range manifest source length is negative or too large")?;
    let wanted_start = offset.min(source_len);
    let wanted_end = offset.saturating_add(len).min(source_len);
    let sql = select_range_manifest_sql();
    let mut statement = tx
        .prepare(&sql)
        .context("prepare range manifest selection")?;
    let rows = statement
        .query_map(
            params![
                nid,
                sqlite_integer(wanted_end, "range end")?,
                sqlite_integer(wanted_start, "range start")?,
                seek_floor(wanted_start)?
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .context("select range manifest")?;

    let mut chunks = Vec::new();
    for row in rows {
        let (hash, chunk_offset, chunk_len) = row.context("decode range manifest row")?;
        chunks.push(decode_manifest_chunk(&nid, hash, chunk_offset, chunk_len)?);
    }
    // Parse, don't validate: the returned type carries the selection
    // contract, and the cdc read performs no second pass (types-friend F5).
    let selected = leyline_cdc::SelectedRange::parse(chunks, source_len, wanted_start, wanted_end)
        .with_context(|| format!("validate selected range manifest for nid {nid}"))?;
    Ok(Some(selected))
}

/// Lower bound for the index seek — see [`OVERLAP_PREDICATE`]. No chunk
/// starting before this point can reach `start`, because CDC caps chunk length
/// at `MAX_CHUNK`.
pub(crate) fn seek_floor(start: usize) -> Result<i64> {
    sqlite_integer(
        start.saturating_sub(leyline_cdc::MAX_CHUNK),
        "range seek floor",
    )
}

/// How many chunks a read of `len` bytes at `offset` would touch. This is the
/// cost of the read, in chunks — the number the whole design exists to keep
/// small. Uses [`OVERLAP_PREDICATE`], so it measures the shipped selection.
pub fn chunks_touched(conn: &Connection, nid: i64, offset: u64, len: usize) -> Result<usize> {
    let start = usize::try_from(offset).context("range offset exceeds usize")?;
    let end = start.saturating_add(len);
    let sql = format!("SELECT COUNT(*) FROM content_manifest WHERE {OVERLAP_PREDICATE}");
    let n: i64 = conn
        .query_row(
            &sql,
            params![
                nid,
                sqlite_integer(end, "range end")?,
                sqlite_integer(start, "range start")?,
                seek_floor(start)?
            ],
            |r| r.get(0),
        )
        .context("count touched chunks")?;
    usize::try_from(n).context("negative touched chunk count")
}

/// Read `buf.len()` bytes at `offset` for `nid`, touching **only** the
/// chunks whose span overlaps the request.
///
/// **Deliberately not public.** This reads the manifest UNCHECKED — it does not
/// consult [`has_chunked_content`], so it will happily serve a stale manifest.
/// The freshness gate is the only thing standing between a missed invalidation
/// and silent data corruption (a deleted file's bytes surfacing in a new file
/// at the same path), and a `pub` unchecked reader is an open invitation to
/// route around it. [`read_content_at`] is the entry point; the compiler now
/// enforces that rather than a doc comment asking nicely.
#[cfg(test)]
pub(crate) fn read_content_chunked(
    conn: &Connection,
    nid: i64,
    buf: &mut [u8],
    offset: u64,
) -> Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let offset = checked_range_offset(offset)?;
    let tx = conn
        .unchecked_transaction()
        .context("begin chunked range read transaction")?;
    let written = read_content_chunked_in_transaction(&tx, nid, buf, offset)?;
    tx.commit()
        .context("commit chunked range read transaction")?;
    Ok(written)
}

fn read_content_chunked_in_transaction(
    tx: &Transaction<'_>,
    nid: i64,
    buf: &mut [u8],
    offset: usize,
) -> Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let Some(selected) = select_range_manifest(tx, nid, offset, buf.len())? else {
        return Ok(0);
    };
    let store = SqliteBlobStore { tx };
    // CANONICAL OPERATION SEAM: SQL selects addresses, BlobStore verifies
    // bytes, and leyline-cdc alone reconstructs them. Do not duplicate any of
    // those operations with a JOIN/copy path here.
    leyline_cdc::read_range_into(&selected, &store, buf).context("reconstruct SQLite chunk range")
}

/// Drop `nid`'s chunk manifest, so subsequent reads fall back to
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
pub(crate) fn invalidate_chunked_content(conn: &Connection, nid: i64) -> Result<()> {
    if !manifest_table_present(conn)? {
        return Ok(());
    }
    conn.execute("DELETE FROM content_manifest WHERE nid = ?1", params![nid])
        .context("invalidate chunk manifest")?;
    Ok(())
}

/// A foreign arena has no chunk tables at all — nothing to invalidate, and
/// probing must not turn into an error on that path.
fn manifest_table_present(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_manifest'",
        [],
        |_| Ok(true),
    )
    .optional()
    .context("probe for content_manifest")
    .map(|present| present.unwrap_or(false))
}

/// Does this arena carry the projection's tree tables — the ones a subtree
/// descent walks?
fn tree_tables_present(conn: &Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
               AND name IN ('nodes','dirs','files')",
            [],
            |r| r.get(0),
        )
        .context("probe for projection tree tables")?;
    Ok(n == 3)
}

/// Invalidate `nid` **and every node beneath it in the tree**.
///
/// The writers that delete or rename a node cascade over descendants, and
/// invalidation has to cascade identically or a child's manifest outlives its
/// `nodes` row. This is the cross-generation-leak guard: an orphaned manifest
/// is not merely stale, it attaches to whatever node next occupies that nid,
/// and `has_chunked_content` would serve the previous occupant's bytes to a
/// brand-new file that was never written. Verified: without this,
/// `write_content(p, "secret")` → `remove_node(p)` → `create_node(p)` →
/// `read_content(p)` returns "secret".
///
/// projection-v5 narrows the hazard without removing it. `files` is
/// append-only, so a nid range can never re-bind to an UNRELATED path the way
/// a reused path-string id could; but a file deleted and re-created at the
/// same path re-binds to the same `file_id`, hence the same nids, so the
/// cascade is still load-bearing.
///
/// Three descent shapes, one per nid class — and never a prefix LIKE, which
/// under the pre-v5 TEXT key both planned as a full scan and (being
/// unanchored) over-matched siblings:
///
/// - **Directory** (`nid < 0`): every file interned beneath it, found by
///   recursing `dirs.parent_dir_id` and mapping the reachable `files` rows to
///   their nid ranges. `dirs`/`files` are append-only, so this is correct
///   whether it runs before or after the caller deletes the `nodes` rows.
/// - **File root** (ordinal 0): the file's whole nid range — a PRIMARY KEY
///   range delete that also sweeps every AST node under it.
/// - **Interior AST node**: recurse `nodes.parent_nid`. This is the one shape
///   that reads `nodes`, so callers must invalidate BEFORE deleting rows.
pub(crate) fn invalidate_chunked_content_subtree(conn: &Connection, nid: i64) -> Result<()> {
    if !manifest_table_present(conn)? {
        return Ok(());
    }
    // A pure content-addressed arena has manifests but no tree tables, so
    // there is no descendant relation to walk — the node stands alone and
    // the single-row delete IS the whole subtree. Probed explicitly rather
    // than caught, so a real SQL fault still surfaces.
    if !tree_tables_present(conn)? {
        return invalidate_chunked_content(conn, nid);
    }
    if let Some(dir_id) = leyline_schema::nid_dir_id(nid) {
        conn.execute(
            "DELETE FROM content_manifest \
              WHERE nid >= 0 AND (nid >> 24) IN ( \
                    WITH RECURSIVE sub(dir_id) AS ( \
                        SELECT ?1 \
                        UNION ALL \
                        SELECT d.dir_id FROM dirs d JOIN sub s ON d.parent_dir_id = s.dir_id \
                    ) \
                    SELECT f.file_id FROM files f JOIN sub s ON f.dir_id = s.dir_id)",
            params![dir_id],
        )
        .context("invalidate chunk manifest for directory subtree")?;
        return Ok(());
    }
    let ordinal = leyline_schema::nid_ordinal(nid).context("non-negative nid has an ordinal")?;
    if ordinal == 0 {
        let file_id = leyline_schema::nid_file_id(nid).context("non-negative nid has a file_id")?;
        let (lo, hi) = leyline_schema::file_nid_range(file_id);
        conn.execute(
            "DELETE FROM content_manifest WHERE nid BETWEEN ?1 AND ?2",
            params![lo, hi],
        )
        .context("invalidate chunk manifest for file range")?;
        return Ok(());
    }
    conn.execute(
        "DELETE FROM content_manifest \
          WHERE nid IN ( \
                WITH RECURSIVE sub(nid) AS ( \
                    SELECT ?1 \
                    UNION ALL \
                    SELECT n.nid FROM nodes n JOIN sub s ON n.parent_nid = s.nid \
                ) \
                SELECT nid FROM sub)",
        params![nid],
    )
    .context("invalidate chunk manifest for node subtree")?;
    Ok(())
}

/// Capture a manifest only while its freshness witness still matches the
/// authoritative `nodes` row.
pub(crate) fn capture_chunked_content(
    conn: &Connection,
    nid: i64,
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

    // An arena with no `content_generation` table cannot witness freshness at
    // all, so every manifest on it must read stale — which the early return
    // models by refusing to capture.
    let generation_infra: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_generation'",
            [],
            |_| Ok(true),
        )
        .optional()
        .context("probe for content_generation")?
        .unwrap_or(false);
    if !generation_infra {
        return Ok(None);
    }

    let witness: Option<(i64, i64, Option<i64>, i64)> = conn
        .query_row(
            "SELECT meta.source_len, meta.source_generation, nodes.size,
                    COALESCE((SELECT g.generation FROM content_generation g
                               WHERE g.nid = nodes.nid), 0)
               FROM content_manifest_meta AS meta
               JOIN nodes ON nodes.nid = meta.nid
              WHERE meta.nid = ?1",
            params![nid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .context("read chunk manifest freshness witness")?;
    let Some((source_len, source_generation, live_len, live_generation)) = witness else {
        return Ok(None);
    };
    if !manifest_witness_is_fresh(source_len, source_generation, live_len, live_generation) {
        return Ok(None);
    }
    let source_len =
        usize::try_from(source_len).context("chunk manifest source length exceeds usize")?;

    let mut statement = conn
        .prepare(
            "SELECT chunk_hash, byte_offset, byte_len
               FROM content_manifest
              WHERE nid = ?1
              ORDER BY seq",
        )
        .context("prepare chunk manifest snapshot")?;
    let rows = statement
        .query_map(params![nid], |row| {
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
        chunks.push(decode_manifest_chunk(&nid, hash, offset, len)?);
    }
    if chunks.is_empty() {
        return Ok(None);
    }

    let mut expected_offset = 0usize;
    for chunk in &chunks {
        anyhow::ensure!(
            chunk.offset == expected_offset,
            "chunk manifest for nid {nid} has a gap or overlap at {expected_offset}"
        );
        expected_offset = expected_offset
            .checked_add(chunk.len)
            .context("chunk manifest length overflow")?;
    }
    anyhow::ensure!(
        expected_offset == source_len,
        "chunk manifest for nid {nid} covers {expected_offset} bytes, expected {source_len}"
    );

    Ok(Some(ChunkManifestSnapshot { chunks, source_len }))
}

/// The Rust arm of [`WITNESS_FRESH_PREDICATE`] — keep the two in agreement;
/// `witness_predicate_arms_agree` drives both over one truth table. `None`
/// for `live_len` models a NULL `nodes.size` (never fresh, types-friend F4).
fn manifest_witness_is_fresh(
    source_len: i64,
    source_generation: i64,
    live_len: Option<i64>,
    live_generation: i64,
) -> bool {
    source_len >= 0 && Some(source_len) == live_len && source_generation == live_generation
}

/// Refresh `nid` after a known edit, but only if this arena already uses
/// chunk storage. A fresh previous manifest enables bounded incremental work;
/// otherwise the authoritative bytes are chunked in full.
pub(crate) fn refresh_chunked_content_after_edit(
    conn: &Connection,
    nid: i64,
    data: &[u8],
    previous: Option<ChunkManifestSnapshot>,
    edit: leyline_cdc::Edit,
    old_len: usize,
) -> Result<RefreshOutcome> {
    if !chunk_schema_present(conn)? {
        return Ok(RefreshOutcome::Skipped);
    }

    let (chunks, rehashed, outcome) = match previous {
        Some(previous) => {
            anyhow::ensure!(
                previous.source_len == old_len,
                "old manifest length {} does not match old record length {old_len}",
                previous.source_len
            );
            let (chunks, stats) = leyline_cdc::rechunk_with_stats(&previous.chunks, data, edit);
            // The rescan window: everything between the kept prefix and the
            // reused tail was hashed by rechunk; only those rows carry new
            // bytes for the store. Exact accounting is pinned by the cdc
            // fuzzer (prefix_kept + tail_reused + rehashed == len).
            let rehashed = stats.prefix_kept..chunks.len() - stats.tail_reused;
            (chunks, rehashed, RefreshOutcome::Incremental(stats))
        }
        None => {
            let chunks = leyline_cdc::chunk(data);
            let all = 0..chunks.len();
            let scanned = data.len();
            (
                chunks,
                all,
                RefreshOutcome::Full {
                    bytes_scanned: scanned,
                },
            )
        }
    };
    store_content_manifest(conn, nid, data, &chunks, rehashed)?;
    Ok(outcome)
}

/// Is chunk-backed content available AND provably fresh for `nid`?
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
/// disclosure: a file re-created at a deleted file's path re-binds to the
/// same append-only `file_id`, hence the same nids, so an orphaned manifest
/// attaches to it — a brand-new, never-written file served a deleted file's
/// bytes.
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
pub(crate) fn has_chunked_content_in_transaction(tx: &Transaction<'_>, nid: i64) -> Result<bool> {
    // content_generation included: an arena with chunk tables but no
    // generation witness reads as "no chunked content" — every read falls
    // back to the record, slow-but-correct, until a store path creates it.
    let tables_present: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
               AND name IN ('content_manifest','content_manifest_meta',\
                            'content_generation','nodes')",
            [],
            |r| r.get(0),
        )
        .context("probe for chunk tables")?;
    if tables_present < 4 {
        return Ok(false);
    }

    // One query: manifest witness joined to the live node row, freshness by
    // the ONE predicate definition (WITNESS_FRESH_PREDICATE — the same
    // string GC reaps on; see its doc for why there is exactly one copy).
    // COALESCE on size: NULL must read as a value we can decode, not a SQL
    // NULL that errors (types-friend F4); -1 also fails the length branch
    // when it is the live length of a fresh-looking witness.
    let sql = format!(
        "SELECT ({WITNESS_FRESH_PREDICATE}), COALESCE(n.size, -1) \
           FROM content_manifest_meta m JOIN nodes n ON n.nid = m.nid \
          WHERE m.nid = ?1"
    );
    let fresh: Option<(bool, i64)> = tx
        .query_row(&sql, params![nid], |r| {
            Ok((r.get::<_, i64>(0)? != 0, r.get(1)?))
        })
        .optional()
        .context("check manifest freshness")?;
    let Some((true, live_size)) = fresh else {
        return Ok(false);
    };
    if live_size == 0 {
        return Ok(true);
    }

    // Witness matches; confirm the manifest actually has spans.
    let has_rows: bool = tx
        .query_row(
            "SELECT 1 FROM content_manifest WHERE nid = ?1 LIMIT 1",
            params![nid],
            |_| Ok(true),
        )
        .optional()
        .context("probe node manifest")?
        .unwrap_or(false);
    Ok(has_rows)
}

pub fn has_chunked_content(conn: &Connection, nid: i64) -> Result<bool> {
    let tx = conn
        .unchecked_transaction()
        .context("begin chunk freshness transaction")?;
    let fresh = has_chunked_content_in_transaction(&tx, nid)?;
    tx.commit().context("commit chunk freshness transaction")?;
    Ok(fresh)
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
    /// No read occurred (an empty destination buffer). Neither path ran and
    /// neither counter moved — previously this fabricated `Record`, which a
    /// caller doing its own tallying would count against the slow path
    /// (types-friend F11).
    Empty,
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
pub fn read_content_at(conn: &Connection, nid: i64, buf: &mut [u8], offset: u64) -> Result<usize> {
    read_content_at_traced(conn, nid, buf, offset).map(|(n, _)| n)
}

/// [`read_content_at`], additionally reporting which path served the read.
pub fn read_content_at_traced(
    conn: &Connection,
    nid: i64,
    buf: &mut [u8],
    offset: u64,
) -> Result<(usize, ContentSource)> {
    read_content_at_traced_with_finish(conn, nid, buf, offset, |tx| {
        tx.commit()
            .context("commit coherent content read transaction")
    })
}

fn read_content_at_traced_with_finish<F>(
    conn: &Connection,
    nid: i64,
    buf: &mut [u8],
    offset: u64,
    finish: F,
) -> Result<(usize, ContentSource)>
where
    F: FnOnce(Transaction<'_>) -> Result<()>,
{
    if buf.is_empty() {
        return Ok((0, ContentSource::Empty));
    }
    let offset = checked_range_offset(offset)?;
    let tx = conn
        .unchecked_transaction()
        .context("begin coherent content read transaction")?;
    let mut staged = vec![0; buf.len()];
    let (n, source) = if has_chunked_content_in_transaction(&tx, nid)? {
        (
            read_content_chunked_in_transaction(&tx, nid, &mut staged, offset)?,
            ContentSource::Chunked,
        )
    } else {
        (
            read_content_from_record_in_transaction(&tx, nid, &mut staged, offset)?,
            ContentSource::Record,
        )
    };
    finish(tx)?;
    buf[..n].copy_from_slice(&staged[..n]);
    match source {
        ContentSource::Chunked => CHUNKED_READS.fetch_add(1, Ordering::Relaxed),
        ContentSource::Record => RECORD_READS.fetch_add(1, Ordering::Relaxed),
        // Unreachable on this path (the empty-buffer early return above),
        // but the exhaustive match forces every future consumer to decide.
        ContentSource::Empty => 0,
    };
    Ok((n, source))
}

/// Legacy path: the whole file lives in `nodes.record`, so serving a range
/// means materializing all of it and slicing. Preserved verbatim for arenas
/// without chunk tables — and kept here, next to the chunked path, so the cost
/// difference between the two is impossible to miss when reading the code.
///
/// `pub(crate)` for symmetry with [`read_content_chunked`]: callers pick a
/// storage generation by accident if both raw readers are reachable. Go through
/// [`read_content_at`], which picks correctly and reports which ran.
fn read_content_from_record_in_transaction(
    tx: &Transaction<'_>,
    nid: i64,
    buf: &mut [u8],
    offset: usize,
) -> Result<usize> {
    let record: Option<String> = tx
        .query_row(
            "SELECT record FROM nodes WHERE nid = ?1",
            params![nid],
            |row| row.get(0),
        )
        .optional()
        .context("read node record")?
        .flatten();
    let Some(data) = record else {
        return Ok(0);
    };
    let bytes = data.as_bytes();
    if offset >= bytes.len() {
        return Ok(0);
    }
    let end = offset.saturating_add(buf.len()).min(bytes.len());
    let n = end - offset;
    buf[..n].copy_from_slice(&bytes[offset..end]);
    Ok(n)
}

fn checked_range_offset(offset: u64) -> Result<usize> {
    let offset = i64::try_from(offset).context("range offset exceeds SQLite INTEGER")?;
    usize::try_from(offset).context("range offset exceeds usize")
}

/// Total byte length of `nid`'s chunked content (manifest sum).
pub fn chunked_content_len(conn: &Connection, nid: i64) -> Result<usize> {
    let n: Option<i64> = conn
        .query_row(
            "SELECT MAX(byte_offset + byte_len) FROM content_manifest WHERE nid = ?1",
            params![nid],
            |r| r.get(0),
        )
        .context("content length")?;
    // try_from, not `as`: a negative MAX(...) from a corrupt manifest row
    // must be an error, not a near-usize::MAX length (types-friend F8 —
    // every sibling decoder in this file already uses the checked form).
    usize::try_from(n.unwrap_or(0)).context("chunked content length is negative or too large")
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_cdc::{MAX_CHUNK, MIN_CHUNK};
    use leyline_core::{BlobStore, ContentAddressed, FsBlobStore, Hash, MemBlobStore};
    use std::cell::Cell;
    use tempfile::tempdir;

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

    /// These tests drive the chunk layer directly, where a nid is just the
    /// manifest's key — there is no `nodes` row and no path to resolve. `n`
    /// only needs to be distinct across nodes within the same test; it is
    /// otherwise opaque.
    fn node(n: i64) -> i64 {
        leyline_schema::file_nid(n, 0)
    }

    /// The store must NOT re-derive carried-over rows from `data` — that is
    /// the whole of ley-line-open-f8ebe7 (a small edit was hashing the entire
    /// file, O(file) instead of O(edit + resync window), while the stat that
    /// should have caught it measured the boundary scanner one layer down).
    ///
    /// Pinned behaviorally rather than by counting: `data` is CORRUPTED in
    /// the carried region. The old implementation hashed those bytes and
    /// errored on the mismatch; the fixed one never reads them (existence
    /// probe only), succeeds, and range reads still serve the TRUE bytes
    /// because blobs are fetched by content address. The same corruption
    /// placed INSIDE the rehash window must still be refused — the
    /// coordinate-honesty backstop survives on exactly the rows that carry
    /// new bytes.
    #[test]
    fn carried_rows_are_probed_not_rehashed_and_the_window_is_still_verified() {
        let conn = db();
        let nid = node(1);
        let body = prng(0xF8EB_E700, 6 * MAX_CHUNK);
        store_content_chunked(&conn, nid, &body).unwrap();
        let snapshot = capture_manifest_rows(&conn, nid);
        assert!(
            snapshot.len() >= 4,
            "fixture must span several chunks; got {}",
            snapshot.len()
        );

        // Same manifest, but the caller's buffer is WRONG everywhere except
        // the declared rehash window (the last two chunks).
        let window_start_row = snapshot.len() - 2;
        let window_byte_start = snapshot[window_start_row].offset;
        let mut corrupted = vec![0u8; body.len()];
        corrupted[window_byte_start..].copy_from_slice(&body[window_byte_start..]);

        let tx = conn.unchecked_transaction().unwrap();
        store_content_manifest_in_transaction(
            &tx,
            nid,
            &corrupted,
            &snapshot,
            window_start_row..snapshot.len(),
        )
        .expect("carried rows must be verified by existence, not by re-hashing caller bytes");
        tx.commit().unwrap();

        // Reads still serve the TRUE bytes: manifests address blobs by hash.
        // (The test-only unchecked reader — this fixture has no `nodes` row,
        // and the freshness gate is not what this test is about.)
        let mut out = vec![0u8; 64];
        let n = read_content_chunked(&conn, nid, &mut out, 0).unwrap();
        assert_eq!(
            &out[..n],
            &body[..n],
            "content addressing ignores the caller's corrupt buffer"
        );

        // The backstop still bites where the bytes are claimed to be new.
        let tx = conn.unchecked_transaction().unwrap();
        let err = store_content_manifest_in_transaction(
            &tx,
            nid,
            &corrupted,
            &snapshot,
            0..snapshot.len(),
        )
        .expect_err("bytes inside the rehash window must still hash-match the manifest");
        assert!(
            err.to_string().contains("does not match"),
            "unexpected error: {err:#}"
        );
    }

    /// Rows of the stored manifest for direct store-layer tests.
    fn capture_manifest_rows(conn: &Connection, nid: i64) -> Vec<leyline_cdc::Chunk> {
        let mut stmt = conn
            .prepare(
                "SELECT chunk_hash, byte_offset, byte_len FROM content_manifest \
                 WHERE nid = ?1 ORDER BY seq",
            )
            .unwrap();
        stmt.query_map(params![nid], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .map(|r| {
            let (h, off, len) = r.unwrap();
            leyline_cdc::Chunk {
                hash: Hash::from_bytes(h.as_slice().try_into().unwrap()),
                offset: usize::try_from(off).unwrap(),
                len: usize::try_from(len).unwrap(),
            }
        })
        .collect()
    }

    fn assert_blob_store_baseline<S: BlobStore>(store: &mut S) {
        let bytes = b"shared contract";
        let hash = store.put(bytes).unwrap();
        assert_eq!(store.put(bytes).unwrap(), hash);
        assert!(store.contains(hash).unwrap());
        assert_eq!(store.get(hash).unwrap().unwrap(), bytes);
        assert!(!store.contains(Hash::ZERO).unwrap());
        assert_eq!(store.get(Hash::ZERO).unwrap(), None);
    }

    struct CountingBlobStore<S> {
        inner: S,
        gets: Cell<usize>,
    }

    impl<S: BlobStore> BlobStore for CountingBlobStore<S> {
        fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
            self.inner.put(bytes)
        }

        fn get(&self, hash: Hash) -> Result<Option<Vec<u8>>> {
            self.gets.set(self.gets.get() + 1);
            self.inner.get(hash)
        }

        fn contains(&self, hash: Hash) -> Result<bool> {
            self.inner.contains(hash)
        }
    }

    #[test]
    fn sqlite_blob_store_matches_shared_backend_baseline() {
        assert_blob_store_baseline(&mut MemBlobStore::new());

        let temp = tempdir().unwrap();
        assert_blob_store_baseline(&mut FsBlobStore::open(temp.path()).unwrap());

        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        assert_blob_store_baseline(&mut SqliteBlobStore { tx: &tx });
    }

    #[test]
    fn sqlite_blob_store_detects_same_key_corruption_but_contains_reports_presence() {
        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        let mut store = SqliteBlobStore { tx: &tx };
        let bytes = b"correct bytes";
        let hash = store.put(bytes).unwrap();

        tx.execute(
            "UPDATE content_chunks SET chunk_bytes = ?1 WHERE chunk_hash = ?2",
            params![b"corrupt bytes", hash.as_bytes().as_slice()],
        )
        .unwrap();

        assert!(store.contains(hash).unwrap());
        let err = store.get(hash).unwrap_err();
        assert!(err.to_string().contains("integrity violation"), "{err:#}");
    }

    #[test]
    fn sqlite_blob_store_detects_valid_bytes_stored_under_the_wrong_key() {
        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        let bytes = b"valid bytes";
        let actual_hash = bytes.as_slice().hash();
        assert_ne!(actual_hash, Hash::ZERO);
        tx.execute(
            "INSERT INTO content_chunks (chunk_hash, chunk_bytes) VALUES (?1, ?2)",
            params![Hash::ZERO.as_bytes().as_slice(), bytes],
        )
        .unwrap();

        let store = SqliteBlobStore { tx: &tx };
        let err = store.get(Hash::ZERO).unwrap_err();
        assert!(err.to_string().contains("integrity violation"), "{err:#}");
    }

    #[test]
    fn sqlite_blob_store_put_rejects_an_already_corrupt_key() {
        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        let bytes = b"correct bytes";
        let hash = bytes.as_slice().hash();
        tx.execute(
            "INSERT INTO content_chunks (chunk_hash, chunk_bytes) VALUES (?1, ?2)",
            params![hash.as_bytes().as_slice(), b"corrupt bytes"],
        )
        .unwrap();

        let mut store = SqliteBlobStore { tx: &tx };
        let err = store.put(bytes).unwrap_err();
        assert!(err.to_string().contains("integrity violation"), "{err:#}");
    }

    #[test]
    fn sqlite_blob_store_selector_returns_only_ordered_overlapping_spans() {
        let conn = db();
        let nid = node(1);
        let data = prng(0x51ec7, MAX_CHUNK * 5);
        store_content_chunked(&conn, nid, &data).unwrap();
        let offset = data.len() / 2;
        let len = 4096;
        let expected = chunks_touched(&conn, nid, offset as u64, len).unwrap();

        let tx = conn.unchecked_transaction().unwrap();
        let selected = select_range_manifest(&tx, nid, offset, len)
            .unwrap()
            .unwrap();

        // SelectedRange::parse already ran inside select_range_manifest —
        // the value's existence IS the validation (types-friend F5). The
        // oracle assertions below check the SQL selection agreed with the
        // Rust-side expectations.
        assert_eq!(selected.chunks().len(), expected);
        assert!(
            selected
                .chunks()
                .windows(2)
                .all(|pair| pair[0].offset < pair[1].offset)
        );
        assert_eq!(selected.len(), len, "clipped interval spans the request");
    }

    #[test]
    fn sqlite_blob_store_selector_rejects_invalid_integer_and_hash_rows() {
        let conn = db();
        let nid = node(1);
        let bytes = b"selected bytes";
        store_content_chunked(&conn, nid, bytes).unwrap();
        conn.execute(
            "UPDATE content_manifest SET chunk_hash = ?1 WHERE nid = ?2",
            params![b"short hash".as_slice(), nid],
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert!(select_range_manifest(&tx, nid, 0, bytes.len()).is_err());
        drop(tx);

        conn.execute(
            "UPDATE content_manifest_meta SET source_len = -1 WHERE nid = ?1",
            params![nid],
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert!(select_range_manifest(&tx, nid, 0, bytes.len()).is_err());
    }

    #[test]
    fn chunks_touched_rejects_offsets_outside_sqlite_integer() {
        let conn = db();
        let offset = u64::try_from(i64::MAX).unwrap() + 1;
        assert!(chunks_touched(&conn, node(1), offset, 1).is_err());
    }

    #[test]
    fn transaction_owned_store_writes_the_manifest_and_reports_chunk_count() {
        let conn = db();
        let nid = node(1);
        let data = prng(0x5eed, MAX_CHUNK * 3);
        let expected_chunks = leyline_cdc::chunk(&data).len();
        assert!(expected_chunks > 1, "fixture must distinguish Ok(0)/Ok(1)");

        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let stored = store_content_chunked_in_transaction(&tx, nid, &data).unwrap();

        assert_eq!(stored, expected_chunks);
        let manifest_rows: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM content_manifest WHERE nid = ?1",
                params![nid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(manifest_rows, expected_chunks as i64);
        let mut round_trip = vec![0_u8; data.len()];
        let selected = select_range_manifest(&tx, nid, 0, data.len())
            .unwrap()
            .unwrap();
        let store = SqliteBlobStore { tx: &tx };
        assert_eq!(
            leyline_cdc::read_range_into(&selected, &store, &mut round_trip).unwrap(),
            data.len()
        );
        assert_eq!(round_trip, data);
        tx.commit().unwrap();
    }

    #[test]
    fn fresh_empty_content_needs_a_witness_but_no_manifest_spans() {
        let conn = db();
        leyline_schema::create_schema(&conn).unwrap();
        let fid = leyline_schema::ensure_file_id(&conn, "empty").unwrap();
        let dir = leyline_schema::ensure_dir_nodes(&conn, "empty", 7).unwrap();
        let nid = leyline_schema::file_nid(fid, 0);
        let name_id = leyline_schema::intern_name(&conn, "empty").unwrap();
        leyline_schema::insert_node(
            &conn,
            nid,
            Some(leyline_schema::dir_nid(dir)),
            Some(name_id),
            None,
            0,
            0,
            0,
            7,
            "",
        )
        .unwrap();

        assert!(!has_chunked_content(&conn, nid).unwrap());
        assert_eq!(store_content_chunked(&conn, nid, &[]).unwrap(), 0);
        let manifest_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_manifest WHERE nid = ?1",
                params![nid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(manifest_rows, 0);
        assert!(
            has_chunked_content(&conn, nid).unwrap(),
            "a fresh zero-length witness is complete without impossible spans"
        );
    }

    #[test]
    fn manifest_freshness_witness_rejects_invalid_lengths() {
        // (source_len, source_generation, live_len, live_generation)
        assert!(manifest_witness_is_fresh(0, 0, Some(0), 0));
        assert!(manifest_witness_is_fresh(5, 3, Some(5), 3));

        assert!(!manifest_witness_is_fresh(-1, 3, Some(-1), 3));
        assert!(!manifest_witness_is_fresh(-1, 3, Some(5), 3));
        assert!(!manifest_witness_is_fresh(5, 3, Some(-1), 3));
        assert!(!manifest_witness_is_fresh(5, 3, Some(4), 3));
        assert!(
            !manifest_witness_is_fresh(5, 3, None, 3),
            "NULL size is never fresh"
        );
        assert!(
            !manifest_witness_is_fresh(5, 3, Some(5), 4),
            "stale generation"
        );
        assert!(
            !manifest_witness_is_fresh(5, -1, Some(5), 0),
            "migrated -1 default never fresh"
        );
    }

    /// F3's mechanism: the Rust arm and WITNESS_FRESH_PREDICATE evaluated by
    /// SQLite must agree on every row of one truth table — including the
    /// NULL-size row (F4) and the (-1, -1) row the previous predicate
    /// triplication got wrong on the shipped gates.
    #[test]
    fn witness_predicate_arms_agree() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (nid INTEGER PRIMARY KEY, size INTEGER);
             CREATE TABLE content_manifest_meta (
                 nid INTEGER PRIMARY KEY,
                 source_len INTEGER NOT NULL,
                 source_generation INTEGER NOT NULL
             );
             CREATE TABLE content_generation (
                 nid INTEGER PRIMARY KEY,
                 generation INTEGER NOT NULL
             );",
        )
        .unwrap();
        let nid = node(1);

        // (source_len, source_generation, live_size, live_generation-row)
        let cases: &[(i64, i64, Option<i64>, Option<i64>)] = &[
            (0, 0, Some(0), None), // gen row absent → live gen 0
            (5, 3, Some(5), Some(3)),
            (-1, 3, Some(-1), Some(3)),
            (5, 3, None, Some(3)),    // NULL size
            (5, 3, Some(5), Some(4)), // stale generation
            (5, -1, Some(5), None),   // migrated default vs absent row (0)
            (5, 0, Some(5), None),    // explicit 0 vs absent row (0) → fresh
        ];
        let sql = format!(
            "SELECT ({WITNESS_FRESH_PREDICATE}) \
               FROM content_manifest_meta m JOIN nodes n ON n.nid = m.nid \
              WHERE m.nid = ?1"
        );
        for &(source_len, source_gen, live_size, live_gen_row) in cases {
            conn.execute_batch("DELETE FROM nodes; DELETE FROM content_manifest_meta; DELETE FROM content_generation;")
                .unwrap();
            conn.execute(
                "INSERT INTO nodes (nid, size) VALUES (?1, ?2)",
                params![nid, live_size],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO content_manifest_meta VALUES (?1, ?2, ?3)",
                params![nid, source_len, source_gen],
            )
            .unwrap();
            if let Some(g) = live_gen_row {
                conn.execute(
                    "INSERT INTO content_generation VALUES (?1, ?2)",
                    params![nid, g],
                )
                .unwrap();
            }
            let sql_verdict: bool = conn
                .query_row(&sql, params![nid], |r| Ok(r.get::<_, i64>(0)? != 0))
                .unwrap();
            let rust_verdict = manifest_witness_is_fresh(
                source_len,
                source_gen,
                live_size,
                live_gen_row.unwrap_or(0),
            );
            assert_eq!(
                sql_verdict, rust_verdict,
                "arms disagree on (len={source_len}, gen={source_gen}, \
                 size={live_size:?}, live_gen={live_gen_row:?})"
            );
        }
    }

    /// THE b82f56 case, end to end: a foreign writer replaces the record
    /// with SAME-LENGTH bytes via raw SQL. Under the (size, mtime) witness
    /// this served the previous occupant's bytes; the generation trigger
    /// fires for any SQL writer, so the read must fall back to the live
    /// record instead of the stale manifest.
    #[test]
    fn same_shape_replacement_by_a_foreign_writer_is_not_served_stale() {
        let conn = db();
        leyline_schema::create_schema(&conn).unwrap();
        // Re-run schema creation now that nodes exists so triggers install.
        create_chunked_content_schema(&conn).unwrap();

        let old = "the first occupant's secret bytes".repeat(1000);
        let nid = insert_node_record_with_mtime(&conn, "n", &old, 7);
        store_content_chunked(&conn, nid, old.as_bytes()).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert!(has_chunked_content_in_transaction(&tx, nid).unwrap());
        tx.commit().unwrap();

        // Foreign writer: same length, same mtime, different bytes, raw SQL.
        let new = "the second occupant's public bytes".repeat(1000)[..old.len()].to_string();
        assert_eq!(new.len(), old.len());
        conn.execute(
            "UPDATE nodes SET record = ?1 WHERE nid = ?2",
            params![new, nid],
        )
        .unwrap();

        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            !has_chunked_content_in_transaction(&tx, nid).unwrap(),
            "a same-shape replacement must invalidate the witness — the \
             generation trigger fires for writers that never heard of this \
             module"
        );
        drop(tx);

        let mut buf = vec![0u8; 64];
        let n = read_content_at(&conn, nid, &mut buf, 0).unwrap();
        assert_eq!(
            &buf[..n],
            &new.as_bytes()[..n],
            "the read must serve the NEW bytes via the record fallback"
        );
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
    fn expected_touched(conn: &Connection, nid: i64, start: usize, len: usize) -> usize {
        let mut stmt = conn
            .prepare("SELECT byte_offset, byte_len FROM content_manifest WHERE nid = ?1")
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map(params![nid], |r| {
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
        let nid = node(1);
        let data = prng(1, 3_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();
        assert_eq!(chunked_content_len(&conn, nid).unwrap(), data.len());

        let mut buf = vec![0u8; data.len()];
        let n = read_content_chunked(&conn, nid, &mut buf, 0).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(buf, data);
    }

    #[test]
    fn range_reads_return_correct_bytes() {
        let conn = db();
        let nid = node(1);
        let data = prng(2, 3_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();

        for &(off, len) in &[
            (0usize, 100usize),
            (1_000_000, 250_000), // straddles many chunks
            (123_456, 4096),
            (data.len() - 10, 10),
        ] {
            let mut buf = vec![0u8; len];
            let n = read_content_chunked(&conn, nid, &mut buf, off as u64).unwrap();
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
        let nid = node(1);
        let data = prng(3, 8_000_000);
        let total = store_content_chunked(&conn, nid, &data).unwrap();
        assert!(total > 50, "need a many-chunk file, got {total}");

        let mid = data.len() / 2;
        let touched = chunks_touched(&conn, nid, mid as u64, 4096).unwrap();
        assert!(
            touched <= 2,
            "a 4KiB read must touch <=2 of {total} chunks, touched {touched} — \
             the SQL layer must not re-materialize the file"
        );

        let tx = conn.unchecked_transaction().unwrap();
        let selected = select_range_manifest(&tx, nid, mid, 4096).unwrap().unwrap();
        let non_overlapping_hash: Vec<u8> = tx
            .query_row(
                "SELECT chunk_hash FROM content_manifest \
                  WHERE nid = ?1 AND (byte_offset + byte_len) <= ?2 \
                  ORDER BY byte_offset LIMIT 1",
                params![nid, i64::try_from(mid).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        tx.execute(
            "UPDATE content_chunks \
                SET chunk_bytes = zeroblob(length(chunk_bytes)) \
              WHERE chunk_hash = ?1",
            params![non_overlapping_hash],
        )
        .unwrap();

        let store = CountingBlobStore {
            inner: SqliteBlobStore { tx: &tx },
            gets: Cell::new(0),
        };
        let mut buf = vec![0u8; 4096];
        let n = leyline_cdc::read_range_into(&selected, &store, &mut buf).unwrap();
        assert_eq!(store.gets.get(), touched);
        assert_eq!(&buf[..n], &data[mid..mid + n]);
    }

    #[test]
    fn read_content_chunked_does_not_fetch_a_corrupt_non_overlapping_blob() {
        let conn = db();
        let nid = node(1);
        let data = prng(0xc0ffee, 2_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();
        let offset = data.len() / 2;
        let (hash, mut bytes): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT manifest.chunk_hash, chunks.chunk_bytes \
                   FROM content_manifest AS manifest \
                   JOIN content_chunks AS chunks USING (chunk_hash) \
                  WHERE manifest.nid = ?1 \
                    AND (manifest.byte_offset + manifest.byte_len) <= ?2 \
                  ORDER BY manifest.byte_offset \
                  LIMIT 1",
                params![nid, i64::try_from(offset).unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        bytes[0] ^= 0xff;
        conn.execute(
            "UPDATE content_chunks SET chunk_bytes = ?1 WHERE chunk_hash = ?2",
            params![bytes, hash],
        )
        .unwrap();

        let mut out = vec![0xa5; 4096];
        let written = read_content_chunked(&conn, nid, &mut out, offset as u64).unwrap();

        assert_eq!(written, out.len());
        assert_eq!(out, data[offset..offset + written]);
    }

    /// The shipped overlap predicate selects EXACTLY the overlapping spans —
    /// no extras. Checked against a Rust-side oracle at boundary-aligned
    /// ranges, where a `<` → `<=` slip would silently pull in an adjacent
    /// zero-overlap chunk: correct bytes, wasted read. Correctness tests
    /// cannot see that; this one can.
    #[test]
    fn overlap_predicate_selects_exactly_the_overlapping_spans() {
        let conn = db();
        let nid = node(1);
        let data = prng(8, 4_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();

        // Every chunk boundary, plus interior and degenerate offsets.
        let mut stmt = conn
            .prepare("SELECT byte_offset, byte_len FROM content_manifest WHERE nid=?1 ORDER BY seq")
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map(params![nid], |r| {
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
            let got = chunks_touched(&conn, nid, off as u64, len).unwrap();
            let want = expected_touched(&conn, nid, off, len);
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

        let na = store_content_chunked(&conn, node(1), &a).unwrap();
        let nb = store_content_chunked(&conn, node(2), &b).unwrap();

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
        let nid = node(1);
        let data = prng(7, 2_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_chunks", [], |r| r.get(0))
            .unwrap();

        let mut edited = data.clone();
        let mid = edited.len() / 2;
        edited.splice(mid..mid, [0xAA, 0xBB, 0xCC]);
        store_content_chunked(&conn, nid, &edited).unwrap();

        // Manifest reflects the new length exactly (old spans gone).
        assert_eq!(chunked_content_len(&conn, nid).unwrap(), edited.len());
        let mut buf = vec![0u8; edited.len()];
        read_content_chunked(&conn, nid, &mut buf, 0).unwrap();
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
        const SEED: u64 = 0x0005_DEEC_E66D_u64;
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
            let nid = node(case as i64 + 1);
            store_content_chunked(&conn, nid, &data).unwrap();

            // (1) the manifest tiles [0, len) exactly.
            let mut stmt = conn
                .prepare(
                    "SELECT byte_offset, byte_len FROM content_manifest                       WHERE nid = ?1 ORDER BY seq",
                )
                .unwrap();
            let spans: Vec<(usize, usize)> = stmt
                .query_map(params![nid], |r| {
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
            assert_eq!(chunked_content_len(&conn, nid).unwrap(), data.len());

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
                let n = read_content_chunked(&conn, nid, &mut buf, off as u64).unwrap();

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

                let got = chunks_touched(&conn, nid, off as u64, want_len).unwrap();
                let want = expected_touched(&conn, nid, off, want_len);
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
        let nid = node(1);
        let data = prng(9, 4_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();
        conn.execute_batch("ANALYZE").unwrap();

        let sql = format!("EXPLAIN QUERY PLAN {}", select_range_manifest_sql());
        let mut stmt = conn.prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(
                params![nid, 100_000i64, 90_000i64, seek_floor(90_000).unwrap()],
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
        let nid = node(1);
        let data = prng(11, 6_000_000);
        store_content_chunked(&conn, nid, &data).unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT byte_offset, byte_len FROM content_manifest \
                  WHERE nid = ?1 ORDER BY seq",
            )
            .unwrap();
        let spans: Vec<(usize, usize)> = stmt
            .query_map(params![nid], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for start in [0, 1, MAX_CHUNK, 1_000_000, 3_000_000, data.len() - 1] {
            let floor = usize::try_from(seek_floor(start).unwrap()).unwrap();

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
            seek_floor(3_000_000).unwrap() > 0,
            "a read 3MB into a file must not seek from offset 0"
        );
    }

    /// Minimal `nodes` row so the record-fallback path has something to
    /// read. Returns the file's nid.
    fn insert_node_record(conn: &Connection, rel_path: &str, content: &str) -> i64 {
        insert_node_record_with_mtime(conn, rel_path, content, 1)
    }

    /// [`insert_node_record`], with an explicit `mtime` — several tests probe
    /// generation-witness behavior that depends on the exact value.
    ///
    /// Builds the fixture through the CANONICAL contract, not a hand-rolled
    /// copy: a fixture that declares its own `nodes` DDL drifts from what
    /// producers ship against — which is how `record JSON`'s NUMERIC
    /// affinity went unnoticed (bead `ley-line-open-f7966d`), and how a
    /// fixture keyed on a TEXT `id` would drift from the v5 integer `nid`
    /// this whole module is keyed on.
    fn insert_node_record_with_mtime(
        conn: &Connection,
        rel_path: &str,
        content: &str,
        mtime: i64,
    ) -> i64 {
        leyline_schema::create_schema(conn).unwrap();
        let file_id = leyline_schema::ensure_file_id(conn, rel_path).unwrap();
        let dir_id = leyline_schema::ensure_dir_nodes(conn, rel_path, mtime).unwrap();
        let nid = leyline_schema::file_nid(file_id, 0);
        let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
        let name_id = leyline_schema::intern_name(conn, name).unwrap();
        leyline_schema::insert_node(
            conn,
            nid,
            Some(leyline_schema::dir_nid(dir_id)),
            Some(name_id),
            None,
            0,
            0,
            content.len() as i64,
            mtime,
            content,
        )
        .unwrap();
        nid
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

        let legacy = insert_node_record(&conn, "legacy", &content); // record only
        let modern = insert_node_record(&conn, "modern", &content); // record AND manifest
        store_content_chunked(&conn, modern, content.as_bytes()).unwrap();

        for &(off, len) in &[
            (0usize, 100usize),
            (12_345, 9_000),
            (39_990, 50),
            (0, 40_000),
        ] {
            let mut a = vec![0u8; len];
            let (na, sa) = read_content_at_traced(&conn, legacy, &mut a, off as u64).unwrap();
            let mut b = vec![0u8; len];
            let (nb, sb) = read_content_at_traced(&conn, modern, &mut b, off as u64).unwrap();

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
        let nid = insert_node_record(&conn, "n", "hello world");

        assert!(!has_chunked_content(&conn, nid).unwrap());

        let mut buf = vec![0u8; 5];
        let (n, src) = read_content_at_traced(&conn, nid, &mut buf, 6).unwrap();
        assert_eq!(&buf[..n], b"world");
        assert_eq!(src, ContentSource::Record);
    }

    #[test]
    fn transaction_completion_error_does_not_mutate_the_destination() {
        let conn = Connection::open_in_memory().unwrap();
        let nid = insert_node_record(&conn, "n", "hello world");
        let mut buf = [0xA5; 5];

        let err = read_content_at_traced_with_finish(&conn, nid, &mut buf, 0, |_| {
            anyhow::bail!("injected transaction completion failure")
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("injected transaction completion failure"),
            "{err:#}"
        );
        assert_eq!(buf, [0xA5; 5]);
    }

    #[test]
    fn record_fallback_rejects_an_unrepresentable_offset_without_mutation() {
        let conn = Connection::open_in_memory().unwrap();
        let nid = insert_node_record(&conn, "n", "hello world");
        let mut buf = [0xA5; 5];

        let err = read_content_at_traced(&conn, nid, &mut buf, u64::MAX).unwrap_err();

        assert!(err.to_string().contains("range offset"), "{err:#}");
        assert_eq!(buf, [0xA5; 5]);
    }

    /// The counters must actually move, or they cannot answer "is the mount
    /// really getting chunked reads?" — the question they exist for.
    #[test]
    fn read_source_counters_track_both_paths() {
        let conn = db();
        let r_nid = insert_node_record(&conn, "r", "abcdefghij");
        let c_nid = insert_node_record(&conn, "c", "abcdefghij");
        store_content_chunked(&conn, c_nid, b"abcdefghij").unwrap();

        let (c0, r0) = read_source_counts();
        let mut buf = vec![0u8; 4];
        read_content_at(&conn, c_nid, &mut buf, 0).unwrap();
        read_content_at(&conn, r_nid, &mut buf, 0).unwrap();
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
        let nid = insert_node_record(&conn, "n", "original-content");
        store_content_chunked(&conn, nid, b"original-content").unwrap();

        let mut buf = vec![0u8; 64];
        let (n, src) = read_content_at_traced(&conn, nid, &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"original-content");
        assert_eq!(
            src,
            ContentSource::Chunked,
            "precondition: served from chunks"
        );

        // A writer with no knowledge of chunk tables replaces the content.
        // Same shape as leyline-ts's reproject: update `record`, bump mtime.
        conn.execute(
            "UPDATE nodes SET record = ?1, size = ?2, mtime = mtime + 1 WHERE nid = ?3",
            params!["REPLACED-by-a-stranger", 21i64, nid],
        )
        .unwrap();

        let mut buf = vec![0u8; 64];
        let (n, src) = read_content_at_traced(&conn, nid, &mut buf, 0).unwrap();
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
        let nid = insert_node_record(&conn, "n", "aaaaaaaa");
        store_content_chunked(&conn, nid, b"aaaaaaaa").unwrap();

        conn.execute(
            "UPDATE nodes SET record = 'bbbbbbbb', mtime = mtime + 1 WHERE nid = ?1",
            params![nid],
        )
        .unwrap();

        let mut buf = vec![0u8; 32];
        let (n, src) = read_content_at_traced(&conn, nid, &mut buf, 0).unwrap();
        assert_eq!(
            &buf[..n],
            b"bbbbbbbb",
            "equal-length rewrite served stale bytes"
        );
        assert_eq!(src, ContentSource::Record);
    }

    fn manifest_rows(conn: &Connection, nid: i64) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM content_manifest WHERE nid = ?1",
            params![nid],
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
        let nid = insert_node_record(&conn, "n", "some content here");
        store_content_chunked(&conn, nid, b"some content here").unwrap();
        assert!(manifest_rows(&conn, nid) > 0, "precondition: rows exist");

        invalidate_chunked_content(&conn, nid).unwrap();
        assert_eq!(
            manifest_rows(&conn, nid),
            0,
            "invalidation left orphaned manifest rows behind"
        );
    }

    /// The directory cascade must reach every file beneath it, at any depth,
    /// found by walking `dirs.parent_dir_id` — never by a string match. That
    /// is what makes a sibling directory whose NAME merely shares a prefix
    /// ("docsibling" vs "docs") safe from the cascade: under the pre-v5
    /// scheme this was an `id LIKE 'x/%'` delete, which both planned as a
    /// full scan and (being unanchored) could over-match exactly this
    /// sibling; the v5 recursive-CTE walk over `dirs`/`files` cannot.
    #[test]
    fn subtree_invalidation_cascades_to_descendants() {
        let conn = db();
        leyline_schema::create_schema(&conn).unwrap();

        let store = |rel_path: &str| -> i64 {
            let file_id = leyline_schema::ensure_file_id(&conn, rel_path).unwrap();
            let dir_id = leyline_schema::ensure_dir_nodes(&conn, rel_path, 1).unwrap();
            let nid = leyline_schema::file_nid(file_id, 0);
            let name = rel_path.rsplit('/').next().unwrap();
            let name_id = leyline_schema::intern_name(&conn, name).unwrap();
            leyline_schema::insert_node(
                &conn,
                nid,
                Some(leyline_schema::dir_nid(dir_id)),
                Some(name_id),
                None,
                0,
                0,
                7,
                1,
                "content",
            )
            .unwrap();
            store_content_chunked(&conn, nid, b"content").unwrap();
            nid
        };

        let readme = store("docs/readme.txt");
        let nested = store("docs/deep/nested.txt");
        let sibling = store("docsibling/file.txt");

        let docs_nid = leyline_schema::resolve_path(&conn, "docs")
            .unwrap()
            .unwrap();
        invalidate_chunked_content_subtree(&conn, docs_nid).unwrap();

        assert_eq!(manifest_rows(&conn, readme), 0, "child not invalidated");
        assert_eq!(
            manifest_rows(&conn, nested),
            0,
            "grandchild not invalidated"
        );
        // Prefix-sibling must NOT be caught: "docs" must not match "docsibling".
        assert!(
            manifest_rows(&conn, sibling) > 0,
            "cascade over-matched a prefix sibling"
        );
    }

    #[test]
    fn range_read_transaction_observes_one_manifest_generation() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coherent.sqlite");
        let reader = Connection::open(&path).unwrap();
        reader.pragma_update(None, "journal_mode", "WAL").unwrap();
        leyline_schema::create_schema(&reader).unwrap();
        create_chunked_content_schema(&reader).unwrap();

        let old = vec![b'a'; 2_000_000];
        let new = vec![b'b'; old.len()];
        let old_record = String::from_utf8(old.clone()).unwrap();
        let file_id = leyline_schema::ensure_file_id(&reader, "n").unwrap();
        let dir_id = leyline_schema::ensure_dir_nodes(&reader, "n", 1).unwrap();
        let nid = leyline_schema::file_nid(file_id, 0);
        let name_id = leyline_schema::intern_name(&reader, "n").unwrap();
        leyline_schema::insert_node(
            &reader,
            nid,
            Some(leyline_schema::dir_nid(dir_id)),
            Some(name_id),
            None,
            0,
            0,
            i64::try_from(old.len()).unwrap(),
            1,
            &old_record,
        )
        .unwrap();
        store_content_chunked(&reader, nid, &old).unwrap();

        let selected = Arc::new(Barrier::new(2));
        let committed = Arc::new(Barrier::new(2));
        let writer_path = path.clone();
        let writer_selected = Arc::clone(&selected);
        let writer_committed = Arc::clone(&committed);
        let writer = thread::spawn(move || {
            let conn = Connection::open(writer_path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            writer_selected.wait();
            let new_record = String::from_utf8(new.clone()).unwrap();
            conn.execute(
                "UPDATE nodes SET record = ?1, size = ?2, mtime = 2 WHERE nid = ?3",
                params![new_record, i64::try_from(new.len()).unwrap(), nid],
            )
            .unwrap();
            store_content_chunked(&conn, nid, &new).unwrap();
            writer_committed.wait();
            new
        });

        let tx = reader.unchecked_transaction().unwrap();
        assert!(has_chunked_content_in_transaction(&tx, nid).unwrap());
        selected.wait();
        committed.wait();
        let mut old_out = vec![0xa5; 256 * 1024];
        let old_written =
            read_content_chunked_in_transaction(&tx, nid, &mut old_out, 64 * 1024).unwrap();
        tx.commit().unwrap();
        let new = writer.join().unwrap();
        assert_eq!(
            &old_out[..old_written],
            &old[64 * 1024..64 * 1024 + old_written],
            "reader mixed generations after writer commit"
        );

        let mut new_out = vec![0xa5; old_out.len()];
        let (new_written, source) =
            read_content_at_traced(&reader, nid, &mut new_out, 64 * 1024).unwrap();
        assert_eq!(source, ContentSource::Chunked);
        assert_eq!(
            &new_out[..new_written],
            &new[64 * 1024..64 * 1024 + new_written],
            "next transaction did not observe the new coherent generation"
        );
    }
}
