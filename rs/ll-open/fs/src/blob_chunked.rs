//! Chunk-backed storage for whole-file `source_blobs` — the second CDC
//! activation target.
//!
//! ## Why a second target exists
//!
//! [`crate::activation`]'s original walk is over `nodes` — construct-granular
//! rows. Measured on a real mache projection (bead `ley-line-open-baa57f`),
//! 395,173 of 395,173 leaf nodes sat below the 8 KiB chunking floor: every
//! manifest described exactly one chunk identical to its source, adding +21%
//! database size (440 MB of manifest/witness overhead for 1.9 MB of dedup).
//! `source_blobs` (ADR-0028) holds whole-FILE content, where chunk-level
//! dedup actually pays; ADR-0028 §2.2 named CDC as the anticipated downstream
//! refinement over exactly that table. ADR-0033 records both targets and the
//! floor policy.
//!
//! ## The load-bearing simplification — existence is freshness
//!
//! `source_blobs` rows are content-addressed and IMMUTABLE by construction:
//! `blob_hash` = BLAKE3(`blob_bytes`), populated by `INSERT OR IGNORE`, so no
//! writer can ever change the bytes under a key — "a new version" of a file
//! is a DIFFERENT row. A blob manifest therefore needs NO freshness witness.
//! The entire apparatus the `nodes` target carries to survive mutation —
//! `content_manifest_meta`, `content_generation`, the writer-proof triggers,
//! the [`crate::chunked::WITNESS_FRESH_PREDICATE`] gate — defends against a
//! writer class that cannot exist here, and is deliberately absent. A blob
//! manifest row's existence IS its freshness proof.
//!
//! The one hazard that remains is DELETION of a blob row, which turns its
//! manifest into garbage — never into wrong bytes, because every read
//! resolves chunks by content address. [`crate::gc`] reaps those manifests
//! and extends its unreachable-chunk predicate over this table, so a chunk
//! referenced only by a blob manifest is never collected.
//!
//! ## The chunk pool is shared
//!
//! Blob chunks go into the same `content_chunks` table as node chunks
//! ([`crate::chunked::CHUNK_POOL_DDL`]), through the same verify-on-read
//! [`crate::chunked::SqliteBlobStore`]. Identical content reached through
//! either target is stored once — cross-target dedup is a property of the
//! pool, not of either manifest.

use anyhow::{Context, Result, ensure};
use leyline_core::{BlobStore, ContentAddressed, Hash};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::chunked::{
    CHUNK_POOL_DDL, SqliteBlobStore, decode_manifest_chunk, seek_floor, sqlite_integer,
};

/// Per-blob manifest: ordered spans into the shared chunk pool.
///
/// Deliberately NO meta table and NO generation/witness tables here: the
/// module docs' immutability argument is what licenses their absence. Adding
/// a witness "for symmetry" would re-import the maintenance surface this
/// target exists to avoid.
pub const BLOB_MANIFEST_DDL: &str = "\
CREATE TABLE IF NOT EXISTS blob_manifest (
    blob_hash   BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    chunk_hash  BLOB    NOT NULL,
    byte_offset INTEGER NOT NULL,
    byte_len    INTEGER NOT NULL,
    PRIMARY KEY (blob_hash, seq)
);

-- The index that makes a range read a WHERE clause rather than a full scan
-- (same discipline as content_manifest_span).
CREATE INDEX IF NOT EXISTS blob_manifest_span
    ON blob_manifest(blob_hash, byte_offset);

-- The index that makes reachability GC one lookup per distinct chunk instead
-- of a full manifest scan per chunk.
CREATE INDEX IF NOT EXISTS blob_manifest_chunk_hash
    ON blob_manifest(chunk_hash);";

/// Create the shared chunk pool + blob manifest tables (idempotent).
///
/// Deliberately does NOT call `create_chunked_content_schema`: that would
/// install the `nodes` witness tables and generation triggers, taxing every
/// future `nodes` write on a database whose operator chose this target
/// precisely to keep the nodes path untouched.
pub fn create_blob_chunked_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CHUNK_POOL_DDL)
        .context("create shared chunk pool")?;
    ensure_blob_manifest_infra(conn)
}

/// Idempotently ensure the blob manifest tables exist. All statements are
/// IF-NOT-EXISTS-shaped and SQLite DDL is transactional, so GC can run this
/// inside its own transaction and a dry run's rollback stays non-mutating —
/// the same contract as [`crate::chunked::ensure_generation_infra`].
pub(crate) fn ensure_blob_manifest_infra(conn: &Connection) -> Result<()> {
    conn.execute_batch(BLOB_MANIFEST_DDL)
        .context("create blob manifest schema")
}

fn blob_label(blob_hash: Hash) -> String {
    format!("blob {blob_hash}")
}

/// Store `bytes` as `blob_hash`'s chunk manifest inside a caller-owned
/// transaction, returning the chunk count.
///
/// The content-address check is the trust base of the witness-free design:
/// nothing downstream ever re-verifies a manifest against `source_blobs`, so
/// a mismatched pair stored here would be served under the wrong identity
/// forever. A mismatch is therefore REFUSED before any row is written —
/// never stored, never "fixed up".
///
/// Complete tiling is proven by [`leyline_cdc::Manifest::parse`] BEFORE the
/// first insert (parse, don't validate): with no witness to refuse a partial
/// manifest at read time, the write path must be the place where an
/// incomplete tiling is unrepresentable.
pub fn store_blob_chunked_in_transaction(
    tx: &Transaction<'_>,
    blob_hash: Hash,
    bytes: &[u8],
) -> Result<usize> {
    ensure!(
        bytes.hash() == blob_hash,
        "{} does not hash to its claimed content address — refusing to store \
         a mismatched blob",
        blob_label(blob_hash)
    );
    let chunks = leyline_cdc::chunk(bytes);
    let manifest = leyline_cdc::Manifest::parse(&chunks)
        .with_context(|| format!("prove complete tiling for {}", blob_label(blob_hash)))?;
    ensure!(
        manifest.source_len() == bytes.len(),
        "{} manifest tiles {} bytes, expected {}",
        blob_label(blob_hash),
        manifest.source_len(),
        bytes.len()
    );

    // Replace rather than assume: an existing manifest for this hash is
    // byte-identical by construction, but DELETE + INSERT is self-healing
    // against a foreign artifact under the same key, and it is what makes
    // activation's repair path a plain re-store.
    tx.execute(
        "DELETE FROM blob_manifest WHERE blob_hash = ?1",
        params![blob_hash.as_bytes().as_slice()],
    )
    .context("clear previous blob manifest")?;

    let mut put_span = tx
        .prepare(
            "INSERT INTO blob_manifest (blob_hash, seq, chunk_hash, byte_offset, byte_len) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .context("prepare blob manifest insert")?;
    let mut store = SqliteBlobStore::new(tx);
    for (seq, c) in chunks.iter().enumerate() {
        let stored = store
            .put(&bytes[c.offset..c.offset + c.len])
            .context("insert blob chunk")?;
        ensure!(
            stored == c.hash,
            "blob chunk manifest hash does not match bytes"
        );
        put_span
            .execute(params![
                blob_hash.as_bytes().as_slice(),
                i64::try_from(seq).context("blob chunk sequence exceeds SQLite INTEGER")?,
                c.hash.as_bytes().as_slice(),
                sqlite_integer(c.offset, "blob chunk offset")?,
                sqlite_integer(c.len, "blob chunk length")?
            ])
            .context("insert blob manifest span")?;
    }
    Ok(chunks.len())
}

/// Is a COMPLETE chunk manifest present for `blob_hash`?
///
/// "Complete" means: manifest rows exist and tile exactly the blob's
/// `byte_len` bytes ([`leyline_cdc::Manifest::parse`]). There is NO witness
/// comparison — that is the module's load-bearing simplification, not an
/// omission: the row is immutable under its content address, so a manifest
/// that tiles it once tiles it forever.
pub fn has_blob_chunked(tx: &Transaction<'_>, blob_hash: Hash) -> Result<bool> {
    let byte_len: Option<i64> = tx
        .query_row(
            "SELECT byte_len FROM source_blobs WHERE blob_hash = ?1",
            params![blob_hash.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .context("read blob byte length")?;
    // No source row: the manifest (if any) is garbage awaiting GC, and there
    // is nothing for a caller to activate against.
    let Some(byte_len) = byte_len else {
        return Ok(false);
    };
    let byte_len = usize::try_from(byte_len).context("blob byte length is negative")?;

    let chunks = select_blob_manifest(tx, blob_hash)?;
    if chunks.is_empty() {
        // A zero-length blob is fully described by zero spans; anything
        // longer with no rows simply has not been activated.
        return Ok(byte_len == 0);
    }
    // The store path writes only proven-complete manifests inside one
    // transaction, so a non-tiling manifest here is a foreign artifact.
    // Reading it as "not chunked" lets activation repair it by re-store;
    // erroring would wedge activation on a row a rewrite fixes.
    match leyline_cdc::Manifest::parse(&chunks) {
        Ok(manifest) => Ok(manifest.source_len() == byte_len),
        Err(_) => Ok(false),
    }
}

fn select_blob_manifest(tx: &Transaction<'_>, blob_hash: Hash) -> Result<Vec<leyline_cdc::Chunk>> {
    let mut statement = tx
        .prepare(
            "SELECT chunk_hash, byte_offset, byte_len
               FROM blob_manifest
              WHERE blob_hash = ?1
              ORDER BY seq",
        )
        .context("prepare blob manifest selection")?;
    let rows = statement
        .query_map(params![blob_hash.as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .context("select blob manifest")?;
    let label = blob_label(blob_hash);
    let mut chunks = Vec::new();
    for row in rows {
        let (hash, offset, len) = row.context("decode blob manifest row")?;
        chunks.push(decode_manifest_chunk(&label, hash, offset, len)?);
    }
    Ok(chunks)
}

/// The blob arm of the overlap predicate. `?1` = blob hash, `?2` = range end,
/// `?3` = range start, `?4` = the seek floor. Same shape and the same
/// seek-floor discipline as `chunked::OVERLAP_PREDICATE` (the `?4` bound
/// exists because `byte_offset + byte_len` cannot drive an index seek, and
/// CDC's `MAX_CHUNK` cap makes `byte_offset >= start - MAX_CHUNK` sound).
/// Kept as its own single definition because the key column differs; every
/// blob range selection MUST use this string, for the same drift reason as
/// the node predicate.
const BLOB_OVERLAP_PREDICATE: &str = "blob_hash = ?1 AND byte_offset >= ?4 \
     AND byte_offset < ?2 AND (byte_offset + byte_len) > ?3";

/// Total byte length the blob's manifest tiles, or `None` when no manifest
/// exists. The manifest is its own length authority: rows are written only
/// after `Manifest::parse` proved `[0, len)` coverage, and the source row is
/// immutable, so `MAX(byte_offset + byte_len)` IS the source length — there
/// is no witness to consult.
fn blob_manifest_len(tx: &Transaction<'_>, blob_hash: Hash) -> Result<Option<usize>> {
    let len: Option<i64> = tx
        .query_row(
            "SELECT MAX(byte_offset + byte_len) FROM blob_manifest WHERE blob_hash = ?1",
            params![blob_hash.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .context("read blob manifest length")?;
    len.map(|len| usize::try_from(len).context("blob manifest length is negative or too large"))
        .transpose()
}

/// Read `range` (clamped to the blob's length) from `blob_hash`'s chunk
/// manifest, touching **only** the manifest rows whose span overlaps the
/// request. Selection is [`BLOB_OVERLAP_PREDICATE`]; span validation and
/// reconstruction belong to [`leyline_cdc::SelectedRange`] /
/// [`leyline_cdc::read_range_into`], and chunk bytes are σ-verified by the
/// pool store — the same canonical operation seam as the node read path.
pub fn read_blob_range(
    conn: &Connection,
    blob_hash: Hash,
    range: std::ops::Range<u64>,
) -> Result<Vec<u8>> {
    let tx = conn
        .unchecked_transaction()
        .context("begin blob range read transaction")?;
    let Some(source_len) = blob_manifest_len(&tx, blob_hash)? else {
        anyhow::bail!(
            "no chunk manifest for {} — activate the source_blobs target first",
            blob_label(blob_hash)
        );
    };
    let start = usize::try_from(range.start).context("blob range start exceeds usize")?;
    let end = usize::try_from(range.end).context("blob range end exceeds usize")?;
    ensure!(start <= end, "blob range {start}..{end} is reversed");
    let wanted_start = start.min(source_len);
    let wanted_end = end.min(source_len);
    if wanted_start == wanted_end {
        tx.commit().context("commit blob range read transaction")?;
        return Ok(Vec::new());
    }

    let sql = format!(
        "SELECT chunk_hash, byte_offset, byte_len \
           FROM blob_manifest \
          WHERE {BLOB_OVERLAP_PREDICATE} \
          ORDER BY byte_offset, seq"
    );
    let mut statement = tx.prepare(&sql).context("prepare blob range selection")?;
    let rows = statement
        .query_map(
            params![
                blob_hash.as_bytes().as_slice(),
                sqlite_integer(wanted_end, "blob range end")?,
                sqlite_integer(wanted_start, "blob range start")?,
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
        .context("select blob range manifest")?;
    let label = blob_label(blob_hash);
    let mut chunks = Vec::new();
    for row in rows {
        let (hash, offset, len) = row.context("decode blob range row")?;
        chunks.push(decode_manifest_chunk(&label, hash, offset, len)?);
    }
    drop(statement);

    let selected = leyline_cdc::SelectedRange::parse(chunks, source_len, wanted_start, wanted_end)
        .with_context(|| format!("validate selected range manifest for {label}"))?;
    let store = SqliteBlobStore::new(&tx);
    let mut out = vec![0; selected.len()];
    leyline_cdc::read_range_into(&selected, &store, &mut out)
        .context("reconstruct blob chunk range")?;
    tx.commit().context("commit blob range read transaction")?;
    Ok(out)
}

/// How many manifest rows a read of `len` bytes at `offset` would touch —
/// the cost of the read, in chunks. Uses [`BLOB_OVERLAP_PREDICATE`], so it
/// measures the shipped selection (the counterpart of
/// [`crate::chunked::chunks_touched`]).
pub fn blob_chunks_touched(
    conn: &Connection,
    blob_hash: Hash,
    offset: u64,
    len: usize,
) -> Result<usize> {
    let start = usize::try_from(offset).context("blob range offset exceeds usize")?;
    let end = start.saturating_add(len);
    let sql = format!("SELECT COUNT(*) FROM blob_manifest WHERE {BLOB_OVERLAP_PREDICATE}");
    let n: i64 = conn
        .query_row(
            &sql,
            params![
                blob_hash.as_bytes().as_slice(),
                sqlite_integer(end, "blob range end")?,
                sqlite_integer(start, "blob range start")?,
                seek_floor(start)?
            ],
            |r| r.get(0),
        )
        .context("count touched blob chunks")?;
    usize::try_from(n).context("negative touched blob chunk count")
}
