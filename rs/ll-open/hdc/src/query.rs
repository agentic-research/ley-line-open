//! Query layer: radius search, density counting, combined-view
//! prefilter, and unbind / cluster-explanation helpers.
//!
//! All queries go through the `popcount_xor` SQL UDF (registered via
//! `sql_udf::register_hdc_udfs`) so the heavy bit-counting runs at
//! C-speed inside SQLite. Host-language code only handles the row
//! decode + sorting/limit at the boundary.
//!
//! The four primary ops:
//!
//! 1. `radius_search` — scopes within Hamming radius r of a query HV
//!    on one layer. Equivalence-class membership query.
//! 2. `density_count` — how many scopes are within radius r. Hotspot
//!    membership test (>1 means the query is in a cluster).
//! 3. `combined_prefilter` — multi-layer top-k pruning via the
//!    pre-computed `_hdc_combined` BLOBs.
//! 4. `unbind_child_at_position` — recover a child hypervector from
//!    a parent's encoded HV. The cluster-explanation primitive that
//!    distinguishes HDC from plain LSH.

use rusqlite::Connection;

use crate::LayerKind;
use crate::util::Hypervector;

/// One row of a radius-search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeMatch {
    pub scope_id: String,
    pub distance: u32,
}

/// Find every scope within Hamming radius `max_distance` of `query` on
/// the given layer. Returns at most `limit` rows, ordered by distance
/// ascending. Uses the `popcount_xor` UDF; caller is responsible for
/// having run `sql_udf::register_hdc_udfs(conn)` once on the connection.
pub fn radius_search(
    conn: &Connection,
    layer: LayerKind,
    query: &Hypervector,
    max_distance: u32,
    limit: usize,
) -> rusqlite::Result<Vec<ScopeMatch>> {
    let mut stmt = conn.prepare_cached(
        "SELECT scope_id, popcount_xor(hv, ?1) AS d \
         FROM _hdc \
         WHERE layer_kind = ?2 \
           AND popcount_xor(hv, ?1) <= ?3 \
         ORDER BY d ASC \
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            query.to_vec(),
            layer.as_str(),
            max_distance as i64,
            limit as i64,
        ],
        |r| {
            Ok(ScopeMatch {
                scope_id: r.get::<_, String>(0)?,
                distance: r.get::<_, i64>(1)? as u32,
            })
        },
    )?;
    rows.collect()
}

/// Count scopes within radius `max_distance` of `query` on the given
/// layer. Used for hotspot-membership ("is this in a cluster?") and
/// for cluster-density measurement.
pub fn density_count(
    conn: &Connection,
    layer: LayerKind,
    query: &Hypervector,
    max_distance: u32,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM _hdc \
         WHERE layer_kind = ?1 \
           AND popcount_xor(hv, ?2) <= ?3",
        rusqlite::params![layer.as_str(), query.to_vec(), max_distance as i64],
        |r| r.get(0),
    )
}

/// Top-K scopes by combined-view distance, regardless of layer.
/// Stage 1 of multi-layer search per math-friend review F: cheap
/// prefilter against the unweighted combined view, then caller
/// applies a weighted per-layer rerank.
pub fn combined_prefilter(
    conn: &Connection,
    query: &Hypervector,
    limit: usize,
) -> rusqlite::Result<Vec<ScopeMatch>> {
    let mut stmt = conn.prepare_cached(
        "SELECT scope_id, popcount_xor(hv, ?1) AS d \
         FROM _hdc_combined \
         ORDER BY d ASC \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![query.to_vec(), limit as i64], |r| {
        Ok(ScopeMatch {
            scope_id: r.get::<_, String>(0)?,
            distance: r.get::<_, i64>(1)? as u32,
        })
    })?;
    rows.collect()
}

// `unbind_child_at_position` and `explain_cluster_centroid` were retired
// in bead `ley-line-open-7b5086` (bundle composition encoder rewrite).
// Both depended on the XOR-bind encoder algebra (`parent = base ⊕ ⊕_i
// rotate_left(child_i, i)`) which made unbind algebraically clean:
// strip base + siblings via XOR, rotate_right by position, recover child.
//
// Under majority-bundle composition there is no exact-recovery unbind
// — bundle is similarity-dampening, not invertible. A future
// `explain_cluster_centroid` redesign would walk an explicit cleanup-
// memory codebook (the set of candidate child HVs) and pick the
// closest match by popcount distance, rather than reconstruct child
// HVs from the parent's algebra.
//
// Removed:
// - `pub fn unbind_child_at_position`
// - `pub fn explain_cluster_centroid<C>`
// - tests: `unbind_recovers_child_at_position_zero/one`,
//   `explain_cluster_centroid_returns_one_tuple_per_arity`,
//   `explain_cluster_centroid_recovers_kinds_on_homogeneous_cluster`,
//   `explain_cluster_centroid_recovers_constant_positions_on_heterogeneous_cluster`,
//   `explain_cluster_centroid_handles_zero_arity`
//
// The other three query primitives (`radius_search`, `density_count`,
// `combined_prefilter`) are unchanged and remain the substrate's
// load-bearing retrieval surface.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        conn_with_schema_and_udfs as fresh_with_udfs, insert_combined_hv as insert_combined,
        insert_layer_hv as insert,
    };
    use crate::util::expand_seed;

    #[test]
    fn radius_search_returns_only_within_radius() {
        let conn = fresh_with_udfs();
        let q = expand_seed(0x42);

        // Insert q itself + a near-copy (1 bit different) + a far HV.
        insert(&conn, "q_self", LayerKind::Ast, &q, 1);
        let mut near = q;
        near[0] ^= 1; // 1 bit difference
        insert(&conn, "near", LayerKind::Ast, &near, 1);
        let far = expand_seed(0x99);
        insert(&conn, "far", LayerKind::Ast, &far, 1);

        // Radius 100 (well below D/2): should include q_self (d=0) and
        // near (d=1), exclude far (d ≈ D/2).
        let matches = radius_search(&conn, LayerKind::Ast, &q, 100, 10).unwrap();
        let names: Vec<&str> = matches.iter().map(|m| m.scope_id.as_str()).collect();
        assert!(names.contains(&"q_self"), "self-match must be included");
        assert!(names.contains(&"near"), "1-bit-near must be included");
        assert!(!names.contains(&"far"), "far HV must NOT be included");
    }

    #[test]
    fn radius_search_orders_by_distance_ascending() {
        let conn = fresh_with_udfs();
        let q = expand_seed(0x42);

        // Insert q-near with 5 bit differences and q-near2 with 1 bit difference.
        let mut near5 = q;
        near5[0] ^= 0xFF >> 3; // 5 bits set in low nibble
        let mut near1 = q;
        near1[0] ^= 1;
        insert(&conn, "near_5", LayerKind::Ast, &near5, 1);
        insert(&conn, "near_1", LayerKind::Ast, &near1, 1);
        insert(&conn, "self", LayerKind::Ast, &q, 1);

        let matches = radius_search(&conn, LayerKind::Ast, &q, 100, 10).unwrap();
        // Distances must be non-decreasing: self (0) < near_1 (1) < near_5 (5).
        assert_eq!(matches[0].scope_id, "self");
        assert_eq!(matches[0].distance, 0);
        assert_eq!(matches[1].scope_id, "near_1");
        assert_eq!(matches[1].distance, 1);
        assert_eq!(matches[2].scope_id, "near_5");
        assert_eq!(matches[2].distance, 5);
    }

    #[test]
    fn radius_search_respects_layer_filter() {
        let conn = fresh_with_udfs();
        let q = expand_seed(0x42);
        // Insert the same HV under two different layers.
        insert(&conn, "in_ast", LayerKind::Ast, &q, 1);
        insert(&conn, "in_module", LayerKind::Module, &q, 1);

        let ast_matches = radius_search(&conn, LayerKind::Ast, &q, 0, 10).unwrap();
        let module_matches = radius_search(&conn, LayerKind::Module, &q, 0, 10).unwrap();

        assert_eq!(ast_matches.len(), 1);
        assert_eq!(ast_matches[0].scope_id, "in_ast");
        assert_eq!(module_matches.len(), 1);
        assert_eq!(module_matches[0].scope_id, "in_module");
    }

    #[test]
    fn radius_search_respects_limit() {
        let conn = fresh_with_udfs();
        let q = expand_seed(1);
        // Insert 20 zero-distance copies of q.
        for i in 0..20 {
            insert(&conn, &format!("scope_{i}"), LayerKind::Ast, &q, 1);
        }
        let matches = radius_search(&conn, LayerKind::Ast, &q, 0, 5).unwrap();
        assert_eq!(matches.len(), 5);
    }

    #[test]
    fn density_count_in_radius() {
        let conn = fresh_with_udfs();
        let q = expand_seed(0x42);
        // 5 zero-distance + 3 distinct (~D/2 distance).
        for i in 0..5 {
            insert(&conn, &format!("clone_{i}"), LayerKind::Ast, &q, 1);
        }
        for i in 0..3 {
            insert(
                &conn,
                &format!("distinct_{i}"),
                LayerKind::Ast,
                &expand_seed(100 + i),
                1,
            );
        }
        // Tight radius: only the 5 clones.
        let count = density_count(&conn, LayerKind::Ast, &q, 100).unwrap();
        assert_eq!(count, 5);
        // Wide radius: everything.
        let count_all = density_count(&conn, LayerKind::Ast, &q, 9999).unwrap();
        assert_eq!(count_all, 8);
    }

    #[test]
    fn combined_prefilter_returns_topk() {
        let conn = fresh_with_udfs();
        let q = expand_seed(0xAA);
        // 3 close scopes + 5 distant ones in _hdc_combined.
        insert_combined(&conn, "close_0", &q, 1);
        let mut near = q;
        near[0] ^= 0x0F;
        insert_combined(&conn, "close_1", &near, 1);
        for i in 0..5 {
            insert_combined(&conn, &format!("far_{i}"), &expand_seed(200 + i), 1);
        }
        let topk = combined_prefilter(&conn, &q, 3).unwrap();
        assert_eq!(topk.len(), 3);
        // First two should be close_0 and close_1.
        assert_eq!(topk[0].scope_id, "close_0");
        assert_eq!(topk[0].distance, 0);
        assert_eq!(topk[1].scope_id, "close_1");
    }

    #[test]
    fn radius_search_empty_table_returns_empty() {
        let conn = fresh_with_udfs();
        let q = expand_seed(1);
        let matches = radius_search(&conn, LayerKind::Ast, &q, 1000, 10).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn density_count_empty_table_returns_zero() {
        let conn = fresh_with_udfs();
        let q = expand_seed(1);
        let count = density_count(&conn, LayerKind::Ast, &q, 1000).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn density_count_respects_layer_filter() {
        // The SQL `WHERE layer_kind = ?1` clause must isolate counts
        // per layer. Same scope_id under two distinct layers must NOT
        // be summed when querying one. Pin so a refactor that drops
        // the layer predicate (or rewrites it as a JOIN) doesn't
        // silently inflate density measurements across layers.
        let conn = fresh_with_udfs();
        let q = expand_seed(0xABCD);
        insert(&conn, "in_ast_1", LayerKind::Ast, &q, 1);
        insert(&conn, "in_ast_2", LayerKind::Ast, &q, 1);
        insert(&conn, "in_module_1", LayerKind::Module, &q, 1);

        let ast_count = density_count(&conn, LayerKind::Ast, &q, 0).unwrap();
        let module_count = density_count(&conn, LayerKind::Module, &q, 0).unwrap();
        assert_eq!(ast_count, 2, "Ast layer must count only Ast rows");
        assert_eq!(module_count, 1, "Module layer must count only Module rows");
    }

    #[test]
    fn combined_prefilter_empty_table_returns_empty() {
        // Symmetric edge case to the radius/density empty-table pins:
        // a fresh DB with no rows in `_hdc_combined` must produce an
        // empty top-K, never propagate a SQL error or panic. This is
        // what a daemon that just started (before any reparse) sees.
        let conn = fresh_with_udfs();
        let q = expand_seed(1);
        let topk = combined_prefilter(&conn, &q, 10).unwrap();
        assert!(topk.is_empty());
    }

    #[test]
    fn radius_search_radius_zero_returns_only_exact_matches() {
        // Radius 0 = Hamming distance ≤ 0 = exact match. A scope
        // 1 bit away from the query must NOT be returned. Pin so a
        // refactor that flipped `<=` to `<` (which would still exclude
        // the same rows) or `<=` to `<` with subtle off-by-one
        // doesn't sneak in.
        let conn = fresh_with_udfs();
        let q = expand_seed(0xCAFE);
        let mut one_bit_off = q;
        one_bit_off[0] ^= 1;

        insert(&conn, "exact", LayerKind::Ast, &q, 1);
        insert(&conn, "one_off", LayerKind::Ast, &one_bit_off, 1);

        let matches = radius_search(&conn, LayerKind::Ast, &q, 0, 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].scope_id, "exact");
        assert_eq!(matches[0].distance, 0);
    }
}
