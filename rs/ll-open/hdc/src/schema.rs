//! SQL schema for the HDC sidecar tables.
//!
//! Four tables, all in the `_hdc*` namespace so they can be cleanly dropped
//! or migrated without touching any existing leyline schema. The daemon's
//! existing tables (`nodes`, `_ast`, `_lsp*`, `node_refs`, `node_defs`,
//! `_source`) are untouched.
//!
//! See bead `ley-line-open-aab015` for the rationale on each column.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Current HDC schema version. Stored in `_meta.hdc_schema_version` once
/// `_meta` exists alongside HDC tables. Bumping this triggers migration on
/// next daemon startup with the `hdc` feature enabled.
pub const HDC_SCHEMA_VERSION: u32 = 1;

/// Create the four HDC sidecar tables if they don't already exist.
///
/// Idempotent: running on an existing schema is a no-op. Safe to call on
/// every daemon startup.
///
/// **Tables created:**
/// - `_hdc(scope_id, layer_kind, hv, basis)` — per-layer hypervectors.
/// - `_hdc_combined(scope_id, hv, basis)` — XOR-bind across layers,
///   used as an unweighted prefilter for top-k pruning.
/// - `_hdc_baseline(layer_kind, median_distance, mad, sample_size,
///   computed_at_ms)` — empirical radius-calibration (per math-friend
///   review B: real codebases are non-iid, calibrated radius beats
///   theoretical D/2 ± √D/2).
/// - `_hdc_subtree_cache(content_hash, hv, ref_count)` — content-hash
///   keyed cache for hierarchical encoding. Required for bidi unbind
///   recovery (per math-friend review G: without the cache, the bind
///   algebra doesn't pay rent and we'd ship simhash+LSH instead).
pub fn create_hdc_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;

         CREATE TABLE IF NOT EXISTS _hdc (
             scope_id   TEXT NOT NULL,
             layer_kind TEXT NOT NULL,
             hv         BLOB NOT NULL,
             basis      INTEGER NOT NULL,
             PRIMARY KEY (scope_id, layer_kind)
         );

         CREATE INDEX IF NOT EXISTS _hdc_layer_basis_idx
             ON _hdc(layer_kind, basis);

         CREATE TABLE IF NOT EXISTS _hdc_combined (
             scope_id  TEXT PRIMARY KEY,
             hv        BLOB NOT NULL,
             basis     INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS _hdc_baseline (
             layer_kind        TEXT PRIMARY KEY,
             median_distance   INTEGER NOT NULL,
             mad               INTEGER NOT NULL,
             sample_size       INTEGER NOT NULL,
             computed_at_ms    INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS _hdc_subtree_cache (
             content_hash BLOB PRIMARY KEY,
             hv           BLOB NOT NULL,
             ref_count    INTEGER NOT NULL DEFAULT 1
         );

         COMMIT;",
    )
    .context("create _hdc* schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{conn_with_schema as fresh_schema_conn, insert_layer_hv};
    use crate::util::ZERO_HV;
    use crate::{LayerKind, D_BYTES};

    /// Assert a `sqlite_master` row of `kind` ("table" or "index") and
    /// `name` exists. Centralizes the SELECT-COUNT(*)>0 dance so the
    /// SQL doesn't drift between tests.
    fn assert_schema_object_exists(conn: &Connection, kind: &str, name: &str) {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type=?1 AND name=?2",
                [kind, name],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "expected {kind} {name} to exist");
    }

    #[test]
    fn create_hdc_schema_is_idempotent() {
        // Running twice on the same connection must succeed both times.
        // CREATE TABLE IF NOT EXISTS is the discipline, but the test
        // exercises it explicitly so a future migration that breaks
        // idempotence (e.g. accidentally adds CREATE INDEX without IF
        // NOT EXISTS) is caught.
        let conn = Connection::open_in_memory().unwrap();
        create_hdc_schema(&conn).unwrap();
        create_hdc_schema(&conn).unwrap();
        // A third run for good measure — if any statement isn't truly
        // idempotent it'll surface here.
        create_hdc_schema(&conn).unwrap();
    }

    #[test]
    fn create_hdc_schema_creates_all_four_tables() {
        let conn = fresh_schema_conn();
        for table in ["_hdc", "_hdc_combined", "_hdc_baseline", "_hdc_subtree_cache"] {
            assert_schema_object_exists(&conn, "table", table);
        }
    }

    #[test]
    fn _hdc_pkey_enforces_layer_uniqueness() {
        // (scope_id, layer_kind) must be the composite primary key.
        // The same scope can have rows in many layers; the same
        // (scope, layer) pair must not have duplicates.
        let conn = fresh_schema_conn();

        // First insert — fine.
        insert_layer_hv(&conn, "fn_foo", LayerKind::Ast, &ZERO_HV, 1);

        // Second insert with same (scope, layer) — must reject. Uses
        // raw `conn.execute` because the helper unwraps; the Err is
        // exactly what we're asserting on.
        let dup = conn.execute(
            "INSERT INTO _hdc(scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["fn_foo", LayerKind::Ast.as_str(), ZERO_HV.to_vec(), 2i64],
        );
        assert!(dup.is_err(), "duplicate (scope, layer) must be rejected");

        // Same scope, different layer — fine.
        insert_layer_hv(&conn, "fn_foo", LayerKind::Module, &ZERO_HV, 1);
    }

    #[test]
    fn _hdc_layer_basis_index_present() {
        // Drift guard: if the index gets dropped from the schema, this
        // test fails loudly. The index is what makes per-layer
        // freshness queries (`WHERE layer_kind=? AND basis>?`) cheap on
        // large corpuses; dropping it silently would degrade throughput
        // without anyone noticing.
        let conn = fresh_schema_conn();
        assert_schema_object_exists(&conn, "index", "_hdc_layer_basis_idx");
    }

    #[test]
    fn _hdc_baseline_keyed_per_layer() {
        let conn = fresh_schema_conn();
        // One baseline per layer is the contract — same layer twice
        // must be rejected.
        conn.execute(
            "INSERT INTO _hdc_baseline VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["ast", 4096i64, 64i64, 10000i64, 1_700_000_000_000i64],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO _hdc_baseline VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["ast", 4000i64, 60i64, 10000i64, 1_700_000_001_000i64],
        );
        assert!(dup.is_err(), "duplicate baseline for same layer must be rejected");

        // Different layer is fine.
        conn.execute(
            "INSERT INTO _hdc_baseline VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["module", 4080i64, 70i64, 10000i64, 1_700_000_002_000i64],
        )
        .unwrap();
    }

    #[test]
    fn _hdc_subtree_cache_content_hash_pkey() {
        // Same content-hash twice must be rejected. The cache uses
        // BLAKE3 of the canonical subtree form as the key; collisions
        // are cryptographically negligible, so a duplicate insert
        // always means the caller is doing something wrong (re-encoding
        // the same subtree without checking the cache).
        let conn = fresh_schema_conn();
        let hash: Vec<u8> = vec![0xAB; 32];
        let hv = vec![0u8; D_BYTES];

        conn.execute(
            "INSERT INTO _hdc_subtree_cache(content_hash, hv) VALUES (?1, ?2)",
            rusqlite::params![hash, hv],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO _hdc_subtree_cache(content_hash, hv) VALUES (?1, ?2)",
            rusqlite::params![hash, hv],
        );
        assert!(dup.is_err(), "duplicate content_hash must be rejected");
    }
}
