//! Σ axiom falsification suite — the `leyline-fs` slice (ADR-0047 §5, bead
//! `ley-line-open-10d71a`).
//!
//! The ρ this crate ships over Σ addresses: the shared chunk pool
//! (`content_chunks`, read through the verify-on-read `SqliteBlobStore`) and
//! blob-manifest reconstruction (`read_blob_range` over `blob_manifest`).
//! `leyline-core`'s test of the same name covers `MemBlobStore` and
//! `FsBlobStore`; `leyline-cli-lib`'s covers `source_blobs`.

#![cfg(feature = "cdc")]

use leyline_core::{ContentAddressed, Hash};
use leyline_fs::blob_chunked::{
    create_blob_chunked_schema, read_blob_range, store_blob_chunked_in_transaction,
};
use proptest::prelude::*;
use rusqlite::{Connection, params};

/// Deterministic pseudo-random bytes (xorshift64): incompressible enough
/// that CDC cuts more than one chunk once a blob passes the floor.
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

fn pool() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    create_blob_chunked_schema(&conn).unwrap();
    conn
}

fn store(conn: &Connection, bytes: &[u8]) -> Hash {
    let h = bytes.hash();
    let tx = conn.unchecked_transaction().unwrap();
    store_blob_chunked_in_transaction(&tx, h, bytes).unwrap();
    tx.commit().unwrap();
    h
}

/// Every `(chunk_hash, chunk_bytes)` row of the pool.
fn chunks(conn: &Connection) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut stmt = conn
        .prepare("SELECT chunk_hash, chunk_bytes FROM content_chunks ORDER BY chunk_hash")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Sizes at the boundaries ADR-0047 §5 names: empty, sub-floor (below the
/// 8 KiB `MIN_CHUNK`), at the floor, and past the 128 KiB `MAX_CHUNK`, so a
/// manifest of one chunk and of several are both read back.
const BOUNDARY_LENS: &[usize] = &[0, 1, 8 * 1024 - 1, 8 * 1024, 128 * 1024 + 1, 300 * 1024];

/// (CA) ρ(σ(v)) = v through the chunk pool and blob-manifest
/// reconstruction: a whole-range read returns exactly the stored bytes, at
/// the boundary sizes and over arbitrary bytes.
#[test]
fn ca_round_trip_every_store() {
    let conn = pool();
    for (seed, &len) in BOUNDARY_LENS.iter().enumerate() {
        let bytes = prng(seed as u64 + 1, len);
        let h = store(&conn, &bytes);
        assert_eq!(
            read_blob_range(&conn, h, 0..len as u64).unwrap(),
            bytes,
            "(CA) a {len}-byte blob reads back whole"
        );
    }
    assert!(
        chunks(&conn).len() > BOUNDARY_LENS.len(),
        "the large blobs must cut into several chunks, or the multi-chunk path went unread"
    );
    proptest!(ProptestConfig::with_cases(32), |(bytes in proptest::collection::vec(any::<u8>(), 0..40_000))| {
        let h = store(&conn, &bytes);
        prop_assert_eq!(read_blob_range(&conn, h, 0..bytes.len() as u64).unwrap(), bytes);
    });
}

/// (CA) as verify-on-read, decade F3: flip one byte of every stored chunk
/// in turn; every read that covers the chunk fails closed, never serving
/// the tampered bytes.
#[test]
fn ca_tampered_payload_is_refused() {
    let conn = pool();
    let bytes = prng(7, 300 * 1024);
    let h = store(&conn, &bytes);
    let rows = chunks(&conn);
    assert!(
        rows.len() > 1,
        "a multi-chunk blob, so each chunk is tampered alone"
    );
    for (hash, original) in rows {
        let mut tampered = original.clone();
        let i = tampered.len() / 2;
        tampered[i] ^= 0x01;
        conn.execute(
            "UPDATE content_chunks SET chunk_bytes = ?2 WHERE chunk_hash = ?1",
            params![hash, tampered],
        )
        .unwrap();
        assert!(
            read_blob_range(&conn, h, 0..bytes.len() as u64).is_err(),
            "a read over a tampered chunk must be refused"
        );
        conn.execute(
            "UPDATE content_chunks SET chunk_bytes = ?2 WHERE chunk_hash = ?1",
            params![hash, original],
        )
        .unwrap();
    }
    assert_eq!(
        read_blob_range(&conn, h, 0..bytes.len() as u64).unwrap(),
        bytes,
        "restored, the blob reads back whole: the refusals were the tamper's"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// (IM) once an address is defined it is fixed: over a sequence of
    /// writes — new blobs, re-writes of earlier ones, blobs that share chunks
    /// — the bytes first stored under each `content_chunks` address never
    /// change, and they still hash to the address.
    #[test]
    fn im_address_is_never_rebound(
        ops in proptest::collection::vec((0u64..6, 0usize..60_000), 1..10),
    ) {
        let conn = pool();
        let mut first: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = Default::default();
        for (seed, len) in ops {
            // Seeds repeat, so later writes re-store earlier bytes and
            // prefixes of them: the chunk pool sees duplicate addresses.
            store(&conn, &prng(seed, len));
            for (hash, bytes) in chunks(&conn) {
                let sigma = bytes.as_slice().hash();
                prop_assert_eq!(sigma.as_bytes().as_slice(), hash.as_slice());
                let seen = first.entry(hash).or_insert_with(|| bytes.clone());
                prop_assert_eq!(&*seen, &bytes, "an address was rebound");
            }
        }
    }
}
