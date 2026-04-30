//! Shared test helpers — single source of truth for the
//! `Connection::open_in_memory + setup` patterns that previously
//! repeated across calibrate.rs, combined.rs, query.rs, sheaf.rs,
//! and sql_udf.rs (six call sites in three variants).
//!
//! `#[cfg(test)]` only — these helpers don't ship in release builds.
//! Use crate-private exports (`pub(crate)`) so external consumers
//! can't depend on them; they're a test-suite implementation detail.

use rusqlite::Connection;

use crate::schema::create_hdc_schema;
use crate::sql_udf::register_hdc_udfs;
use crate::{Hypervector, LayerKind, D_BITS, D_BYTES};

/// Open an in-memory SQLite connection with the HDC schema applied
/// but no UDFs registered. Tests that exercise schema/storage logic
/// without computing distances (e.g., calibration, combined-view
/// composition) should use this — registering UDFs is a non-trivial
/// per-connection cost that's wasted when the test never calls them.
pub(crate) fn conn_with_schema() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    create_hdc_schema(&conn).unwrap();
    conn
}

/// Open an in-memory SQLite connection with HDC UDFs registered but
/// no schema. Tests that exercise the UDFs in isolation (popcount_xor,
/// BUNDLE, BUNDLE_MAJORITY) should use this — the schema is a
/// distraction when the test creates ad-hoc tables anyway.
pub(crate) fn conn_with_udfs() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    register_hdc_udfs(&conn).unwrap();
    conn
}

/// Open an in-memory SQLite connection with both schema AND UDFs.
/// Tests that exercise the full query layer (radius_search,
/// density_count, etc.) need both. Replaces query.rs's `fresh_with_udfs`
/// helper which was the original spelling.
pub(crate) fn conn_with_schema_and_udfs() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    create_hdc_schema(&conn).unwrap();
    register_hdc_udfs(&conn).unwrap();
    conn
}

/// Insert a per-layer hypervector into `_hdc`. Replaces the
/// byte-identical `insert` / `insert_layer` / `insert_layer_hv`
/// helpers that calibrate.rs, combined.rs, and query.rs each
/// re-implemented under different names.
pub(crate) fn insert_layer_hv(
    conn: &Connection,
    scope: &str,
    layer: LayerKind,
    hv: &Hypervector,
    basis: i64,
) {
    conn.execute(
        "INSERT INTO _hdc(scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![scope, layer.as_str(), hv.to_vec(), basis],
    )
    .unwrap();
}

/// Insert a combined-view hypervector into `_hdc_combined`. Used by
/// query-layer tests that exercise the prefilter table.
pub(crate) fn insert_combined_hv(
    conn: &Connection,
    scope: &str,
    hv: &Hypervector,
    basis: i64,
) {
    conn.execute(
        "INSERT INTO _hdc_combined(scope_id, hv, basis) VALUES (?1, ?2, ?3)",
        rusqlite::params![scope, hv.to_vec(), basis],
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_with_schema_creates_hdc_tables() {
        // Pin: the schema-only helper applies create_hdc_schema. Sample
        // one of the tables (`_hdc`) to verify it exists. Catches a
        // refactor that accidentally drops the schema setup.
        let conn = conn_with_schema();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = '_hdc' AND type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "_hdc table must exist after conn_with_schema()");
    }

    #[test]
    fn conn_with_udfs_registers_popcount_xor() {
        // Pin: the UDF-only helper registers all three UDFs. Probe one
        // (`popcount_xor`) by calling it on two trivial blobs.
        let conn = conn_with_udfs();
        let zeros = vec![0u8; D_BYTES];
        let ones = vec![0xFFu8; D_BYTES];
        let dist: i64 = conn
            .query_row("SELECT popcount_xor(?1, ?2)", [zeros, ones], |r| r.get(0))
            .unwrap();
        assert_eq!(dist as usize, D_BITS);
    }

    #[test]
    fn insert_combined_hv_writes_to_combined_table() {
        // Mirrors the per-layer insert_layer_hv self-test discipline:
        // the combined-view fixture helper must round-trip through
        // _hdc_combined so a refactor that mistakenly targets _hdc
        // (or the wrong column order) is caught immediately.
        use crate::util::expand_seed;
        let conn = conn_with_schema();
        let hv = expand_seed(0xC0DE);
        insert_combined_hv(&conn, "fn_x", &hv, 7);

        let (got_hv, got_basis): (Vec<u8>, i64) = conn
            .query_row(
                "SELECT hv, basis FROM _hdc_combined WHERE scope_id = ?1",
                ["fn_x"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(got_hv, hv.to_vec());
        assert_eq!(got_basis, 7);
    }

    #[test]
    fn conn_with_schema_and_udfs_provides_both() {
        // Pin: combined helper applies schema AND registers UDFs.
        // Catches a refactor that accidentally calls only one.
        let conn = conn_with_schema_and_udfs();
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = '_hdc' AND type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1);
        // UDF probe — query through the schema's _hdc table just for
        // realism (zero-row aggregate).
        let null_bundle: rusqlite::types::Value = conn
            .query_row("SELECT BUNDLE_MAJORITY(hv) FROM _hdc", [], |r| r.get(0))
            .unwrap();
        assert!(matches!(null_bundle, rusqlite::types::Value::Null));
    }
}
