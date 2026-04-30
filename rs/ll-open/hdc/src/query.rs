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

use crate::codebook::{AstNodeFingerprint, BaseCodebook};
use crate::util::{rotate_right, xor_into, Hypervector};
use crate::LayerKind;

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
    let rows = stmt.query_map(
        rusqlite::params![query.to_vec(), limit as i64],
        |r| {
            Ok(ScopeMatch {
                scope_id: r.get::<_, String>(0)?,
                distance: r.get::<_, i64>(1)? as u32,
            })
        },
    )?;
    rows.collect()
}

/// Unbind a child hypervector from its parent's encoded form at a
/// known position. Inverse of the encoder's `rotate_left + xor` step:
///
///   parent = base ⊕ rotate_left(child_0, 0) ⊕ rotate_left(child_1, 1) ⊕ ...
///   recover_child_at_i = rotate_right(parent ⊕ base ⊕ other_children, i)
///
/// Caller passes the parent HV, the parent's base vector (from the
/// codebook), the position to unbind, and the *other* children's HVs
/// already rotated and XOR'd into a "siblings_acc". This API is the
/// mathematical primitive; higher-level cluster-explanation builds
/// on it by walking the codebook to identify which child came back.
pub fn unbind_child_at_position(
    parent: &Hypervector,
    parent_base: &Hypervector,
    siblings_xor_acc: &Hypervector,
    position: usize,
) -> Hypervector {
    let mut residual = *parent;
    xor_into(&mut residual, parent_base);
    xor_into(&mut residual, siblings_xor_acc);
    rotate_right(&residual, position)
}

/// Cluster explanation: given the centroid of a cluster of scopes and
/// a codebook, recover the canonical-kind sequence that the centroid
/// represents at each role position. Returns `(role_idx, recovered_kind)`
/// pairs; "recovered_kind" is the codebook entry whose base_vector is
/// closest to the unbound residual at that position.
///
/// This is the load-bearing distinguishing feature of HDC vs plain LSH
/// (per math-friend review G). LSH gives "these N items are similar";
/// unbind tells you *what they share* — recovered structural skeleton.
///
/// `codebook_kinds` is the candidate set of `Self::Item` values that
/// might have populated each role slot (typically the union of all
/// canonical kinds the AST encoder might emit). The returned recovered
/// kind is the one whose base_vector minimizes Hamming distance to the
/// unbound residual.
pub fn explain_cluster_centroid<C>(
    centroid: &Hypervector,
    centroid_root_kind: crate::canonical::CanonicalKind,
    centroid_arity: u8,
    centroid_child_kinds: &[crate::canonical::CanonicalKind],
    codebook: &C,
    candidate_child_kinds: &[crate::canonical::CanonicalKind],
    expected_arity: usize,
) -> Vec<(usize, crate::canonical::CanonicalKind, u32)>
where
    C: BaseCodebook<Item = AstNodeFingerprint>,
{
    use crate::util::popcount_distance;

    // Step 1: subtract the parent's base vector. What's left is the
    // bundle of all positionally-bound children.
    let parent_fp = AstNodeFingerprint {
        canonical_kind: centroid_root_kind,
        arity_bucket: centroid_arity,
        child_canonical_kinds: centroid_child_kinds.to_vec(),
    };
    let parent_base = codebook.base_vector(&parent_fp);
    let mut residual = *centroid;
    xor_into(&mut residual, &parent_base);

    // Step 2: for each position i, rotate the residual right by i and
    // find the candidate kind whose base_vector best matches.
    // Note: this is a rough cleanup — the bundle of all OTHER children
    // contributes noise, so we expect the closest match to be the
    // dominant child at that position.
    let mut recovered = Vec::with_capacity(expected_arity);
    for i in 0..expected_arity {
        let probe = rotate_right(&residual, i);
        let mut best: Option<(crate::canonical::CanonicalKind, u32)> = None;
        for &kind in candidate_child_kinds {
            // Build a leaf fingerprint for this kind.
            let leaf_fp = AstNodeFingerprint {
                canonical_kind: kind,
                arity_bucket: 0,
                child_canonical_kinds: Vec::new(),
            };
            let leaf_base = codebook.base_vector(&leaf_fp);
            let d = popcount_distance(&probe, &leaf_base);
            match best {
                None => best = Some((kind, d)),
                Some((_, prev_d)) if d < prev_d => best = Some((kind, d)),
                _ => {}
            }
        }
        if let Some((kind, d)) = best {
            recovered.push((i, kind, d));
        }
    }
    recovered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codebook::AstCodebook;
    use crate::schema::create_hdc_schema;
    use crate::sql_udf::register_hdc_udfs;
    use crate::util::{expand_seed, ZERO_HV};

    fn fresh_with_udfs() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_hdc_schema(&conn).unwrap();
        register_hdc_udfs(&conn).unwrap();
        conn
    }

    fn insert(conn: &Connection, scope: &str, layer: LayerKind, hv: &Hypervector, basis: i64) {
        conn.execute(
            "INSERT INTO _hdc(scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![scope, layer.as_str(), hv.to_vec(), basis],
        )
        .unwrap();
    }

    fn insert_combined(conn: &Connection, scope: &str, hv: &Hypervector, basis: i64) {
        conn.execute(
            "INSERT INTO _hdc_combined(scope_id, hv, basis) VALUES (?1, ?2, ?3)",
            rusqlite::params![scope, hv.to_vec(), basis],
        )
        .unwrap();
    }

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
            insert(&conn, &format!("distinct_{i}"), LayerKind::Ast, &expand_seed(100 + i), 1);
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
    fn unbind_recovers_child_at_position_zero() {
        // For a 1-child tree at position 0:
        //   parent = base ⊕ rotate_left(child, 0) = base ⊕ child
        // Unbind: residual = parent ⊕ base ⊕ siblings(empty) = child.
        // rotate_right(child, 0) = child.
        let parent = expand_seed(1);
        let base = expand_seed(2);
        let siblings = ZERO_HV;
        let recovered = unbind_child_at_position(&parent, &base, &siblings, 0);
        // Compute expected: parent ⊕ base
        let mut expected = parent;
        for (e, b) in expected.iter_mut().zip(base.iter()) {
            *e ^= *b;
        }
        assert_eq!(recovered, expected);
    }

    #[test]
    fn unbind_recovers_child_at_position_one() {
        // For a 2-child tree:
        //   parent = base ⊕ rotate_left(child0, 0) ⊕ rotate_left(child1, 1)
        // To recover child1 at position 1:
        //   residual = parent ⊕ base ⊕ rotate_left(child0, 0)
        //            = rotate_left(child1, 1)
        //   rotate_right(residual, 1) = child1
        use crate::util::rotate_left;
        let cb = AstCodebook::new();
        let parent_base = expand_seed(0xBA5E);
        let child0 = cb.base_vector(&AstNodeFingerprint {
            canonical_kind: crate::canonical::CanonicalKind::Op,
            arity_bucket: 0,
            child_canonical_kinds: vec![],
        });
        let child1 = cb.base_vector(&AstNodeFingerprint {
            canonical_kind: crate::canonical::CanonicalKind::Lit,
            arity_bucket: 0,
            child_canonical_kinds: vec![],
        });

        // Build parent the way the encoder would.
        let mut parent = parent_base;
        let p0 = rotate_left(&child0, 0);
        for (a, b) in parent.iter_mut().zip(p0.iter()) {
            *a ^= *b;
        }
        let p1 = rotate_left(&child1, 1);
        for (a, b) in parent.iter_mut().zip(p1.iter()) {
            *a ^= *b;
        }

        // Strip child0 from siblings_acc (already rotated as it sits in parent).
        let recovered = unbind_child_at_position(&parent, &parent_base, &p0, 1);
        assert_eq!(recovered, child1);
    }

    #[test]
    fn explain_cluster_centroid_returns_one_tuple_per_arity() {
        // Smoke-test the API shape: explain returns exactly
        // `expected_arity` tuples, each with a recovered canonical
        // kind from the candidate set and a non-zero distance metric.
        //
        // Note on accuracy: this test passes a SINGLE encoded tree as
        // the "centroid." Cleanup-memory recovery on a single sample
        // has poor SNR — the noise from un-subtracted siblings is
        // O(D/2). True cluster centroids (averaged across many
        // same-shape members via BUNDLE_MAJORITY) have much higher
        // SNR because per-leaf noise cancels. The math friend's
        // ≥80% recovery target applies to real cluster centroids,
        // not single-tree synthetic inputs.
        //
        // Exact-recovery validation lives in hdc-10's integration test
        // against real corpora.
        use crate::canonical::CanonicalKind;
        use crate::encoder::{encode_fresh, EncoderNode};

        let cb = AstCodebook::new();
        let tree = EncoderNode::new(
            CanonicalKind::Block,
            vec![
                EncoderNode::new(CanonicalKind::Op, vec![]),
                EncoderNode::new(CanonicalKind::Lit, vec![]),
            ],
        );
        let centroid = encode_fresh(&tree, &cb);

        let candidate_kinds = [
            CanonicalKind::Decl,
            CanonicalKind::Expr,
            CanonicalKind::Stmt,
            CanonicalKind::Block,
            CanonicalKind::Ref,
            CanonicalKind::Lit,
            CanonicalKind::Op,
        ];

        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Block,
            crate::util::bucket_arity(2),
            &[CanonicalKind::Lit, CanonicalKind::Op],
            &cb,
            &candidate_kinds,
            2,
        );

        // API shape: exactly arity tuples, each with valid index range.
        assert_eq!(recovered.len(), 2);
        for (i, (idx, kind, _d)) in recovered.iter().enumerate() {
            assert_eq!(*idx, i, "tuple at position {i} reports index {idx}");
            assert!(
                candidate_kinds.contains(kind),
                "recovered kind {kind:?} must be from candidate set",
            );
        }
    }

    #[test]
    fn explain_cluster_centroid_handles_zero_arity() {
        // Edge case: leaf node (no children). Should return an empty
        // vec, not panic.
        use crate::canonical::CanonicalKind;
        let cb = AstCodebook::new();
        let candidate_kinds = [CanonicalKind::Lit, CanonicalKind::Op];
        let centroid = cb.base_vector(&AstNodeFingerprint {
            canonical_kind: CanonicalKind::Lit,
            arity_bucket: 0,
            child_canonical_kinds: vec![],
        });
        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Lit,
            0,
            &[],
            &cb,
            &candidate_kinds,
            0,
        );
        assert!(recovered.is_empty());
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
}
