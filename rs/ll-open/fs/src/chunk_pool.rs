//! The shared content-addressed chunk pool behind CDC storage.
//!
//! `content_chunks` holds every chunk once, keyed by `σ = BLAKE3(bytes)`, and
//! lives inside the one `.db` beside `source_blobs`, so an arena stays a single
//! portable file. [`crate::blob_chunked`] stores whole-file `source_blobs`
//! content into it and reads ranges back out of it.
//!
//! ## Canonical operation ownership
//!
//! SQL in the manifest modules selects ordered `(hash, offset, len)`
//! addresses; [`SqliteBlobStore`] owns physical chunk access and
//! verify-on-read; [`leyline_cdc::read_range_into`] owns span validation and
//! reconstruction. A SQL JOIN that also returns `chunk_bytes`, or a second
//! copy loop beside the store, duplicates an owned operation and is an
//! architecture violation rather than an alternative implementation.
//!
//! ## Authority
//!
//! These tables are a DERIVED accelerator, not an identity domain. They have
//! no root and are not a cross-process contract; `source_blobs` holds the
//! authoritative bytes (ADR-0032 §D4, ADR-0033).

use anyhow::{Context, Result, ensure};
use leyline_core::{BlobStore, ContentAddressed, Hash};
use rusqlite::{OptionalExtension, Transaction, params};

/// The shared content-addressed chunk pool. Mirrors the `source_blobs` shape
/// so everything durable stays inside the one `.db`.
pub(crate) const CHUNK_POOL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS content_chunks (
    chunk_hash  BLOB PRIMARY KEY,
    chunk_bytes BLOB NOT NULL,
    -- Named chunk_len, not byte_len: a manifest's byte_len joins against
    -- this table, and a shared name makes the join predicate ambiguous.
    chunk_len   INTEGER GENERATED ALWAYS AS (length(chunk_bytes)) STORED
);";

/// [`BlobStore`] over `content_chunks`, scoped to one caller-owned
/// transaction so a manifest selection and the chunk reads it drives observe
/// one snapshot.
pub(crate) struct SqliteBlobStore<'tx, 'conn> {
    tx: &'tx Transaction<'conn>,
}

impl<'tx, 'conn> SqliteBlobStore<'tx, 'conn> {
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

/// Decode one manifest row into a [`leyline_cdc::Chunk`], refusing a hash of
/// the wrong length or a negative offset/length.
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

/// Lower bound for a manifest's span-index seek. A range read selects rows
/// with `byte_offset >= seek_floor(start)`; that restores a real range seek
/// and is sound because CDC clamps every chunk to at most
/// [`leyline_cdc::MAX_CHUNK`] bytes: a chunk overlapping `start` satisfies
/// `byte_offset + byte_len > start` and `byte_len <= MAX_CHUNK`, hence
/// `byte_offset > start - MAX_CHUNK`. The saturating subtraction keeps offset
/// 0 included.
pub(crate) fn seek_floor(start: usize) -> Result<i64> {
    sqlite_integer(
        start.saturating_sub(leyline_cdc::MAX_CHUNK),
        "range seek floor",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_cdc::MAX_CHUNK;
    use leyline_core::{FsBlobStore, MemBlobStore};
    use rusqlite::Connection;
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
        c.execute_batch(CHUNK_POOL_DDL).unwrap();
        c
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

    #[test]
    fn sqlite_blob_store_matches_shared_backend_baseline() {
        assert_blob_store_baseline(&mut MemBlobStore::new());

        let temp = tempdir().unwrap();
        assert_blob_store_baseline(&mut FsBlobStore::open(temp.path()).unwrap());

        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        assert_blob_store_baseline(&mut SqliteBlobStore::new(&tx));
    }

    #[test]
    fn sqlite_blob_store_detects_same_key_corruption_but_contains_reports_presence() {
        let conn = db();
        let tx = conn.unchecked_transaction().unwrap();
        let mut store = SqliteBlobStore::new(&tx);
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

        let store = SqliteBlobStore::new(&tx);
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

        let mut store = SqliteBlobStore::new(&tx);
        let err = store.put(bytes).unwrap_err();
        assert!(err.to_string().contains("integrity violation"), "{err:#}");
    }

    #[test]
    fn decode_manifest_chunk_rejects_a_short_hash_and_negative_coordinates() {
        let hash = b"abc".as_slice().hash();
        let good = decode_manifest_chunk(&"x", hash.as_bytes().to_vec(), 7, 9).unwrap();
        assert_eq!((good.hash, good.offset, good.len), (hash, 7, 9));
        assert!(decode_manifest_chunk(&"x", b"short".to_vec(), 0, 1).is_err());
        assert!(decode_manifest_chunk(&"x", hash.as_bytes().to_vec(), -1, 1).is_err());
        assert!(decode_manifest_chunk(&"x", hash.as_bytes().to_vec(), 0, -1).is_err());
    }

    #[test]
    fn seek_floor_is_sound_and_tight() {
        let data = prng(11, 6_000_000);
        let spans: Vec<(usize, usize)> = leyline_cdc::chunk(&data)
            .iter()
            .map(|c| (c.offset, c.len))
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

        assert!(
            seek_floor(3_000_000).unwrap() > 0,
            "a read 3MB into a file must not seek from offset 0"
        );
    }
}
