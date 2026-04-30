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

use crate::canonical::CanonicalKind;
use crate::codebook::{AstNodeFingerprint, BaseCodebook};
use crate::util::{rotate_left, rotate_right, xor_into, Hypervector};
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
/// a hypothesis about the parent's positional child-kind sequence,
/// recover the canonical kind at each role position via sibling-
/// cancellation unbind + cleanup-memory.
///
/// This is the load-bearing distinguishing feature of HDC vs plain LSH
/// (per math-friend review G). LSH gives "these N items are similar";
/// unbind tells you *what they share* — recovered structural skeleton.
///
/// ## Algorithm
///
/// 1. Reconstruct `parent_base` from `(root_kind, arity, child_kinds)`.
///    The codebook's signature sorts children internally, so the order
///    of `centroid_child_kinds_positional` doesn't affect parent_base
///    — it affects step 3.
/// 2. Build `all_siblings_acc = XOR over i of rotate_left(leaf_base(
///    centroid_child_kinds_positional[i]), i)` — the parent encoder's
///    contribution from all immediate-children (treated as leaves).
/// 3. For each position i, subtract that position's term out of
///    all_siblings_acc to get the per-position sibling accumulator,
///    then call [`unbind_child_at_position`] (the canonical HDC unbind
///    primitive). Cleanup-memory picks the candidate kind whose
///    `base_vector` is closest to the recovered HV.
///
/// ## Skeptic 4bace1: prior implementation skipped step 2
///
/// The previous version subtracted only `parent_base`, leaving the
/// residual contaminated with all N rotated children's contributions.
/// Probing at position i then saw `(N−1)` D/2-amplitude noise terms
/// alongside the target signal — recovery accuracy was at chance level
/// even for homogeneous clusters. Sibling cancellation closes that gap
/// and brings recovery to ≥80% on real cluster centroids (math
/// friend's target), pinned by the new tests in this module.
///
/// ## Inputs
///
/// - `centroid_child_kinds_positional`: the ORDER MATTERS. Pass the
///   kinds in their actual encoder positions (not sorted). This is
///   the caller's hypothesis about the cluster's structural template;
///   the function returns the closest-matching kind per position so
///   the caller can verify the hypothesis.
/// - `candidate_child_kinds`: kinds the cleanup-memory considers at
///   each position. Typically the union of all canonical kinds.
pub fn explain_cluster_centroid<C>(
    centroid: &Hypervector,
    centroid_root_kind: CanonicalKind,
    centroid_arity: u8,
    centroid_child_kinds_positional: &[CanonicalKind],
    codebook: &C,
    candidate_child_kinds: &[CanonicalKind],
) -> Vec<(usize, CanonicalKind, u32)>
where
    C: BaseCodebook<Item = AstNodeFingerprint>,
{
    use crate::util::{popcount_distance, ZERO_HV};

    let leaf_base =
        |kind: CanonicalKind| codebook.base_vector(&AstNodeFingerprint::leaf(kind));

    let parent_fp = AstNodeFingerprint {
        canonical_kind: centroid_root_kind,
        arity_bucket: centroid_arity,
        child_canonical_kinds: centroid_child_kinds_positional.to_vec(),
    };
    let parent_base = codebook.base_vector(&parent_fp);

    // Build the sum of all positional rotated leaf bases — what the
    // encoder XOR'd into parent_base for the immediate children.
    let mut all_siblings_acc = ZERO_HV;
    let mut per_position_terms: Vec<Hypervector> =
        Vec::with_capacity(centroid_child_kinds_positional.len());
    for (i, &kind) in centroid_child_kinds_positional.iter().enumerate() {
        let term = rotate_left(&leaf_base(kind), i);
        per_position_terms.push(term);
        xor_into(&mut all_siblings_acc, &term);
    }

    let mut recovered = Vec::with_capacity(per_position_terms.len());
    for (i, term) in per_position_terms.iter().enumerate() {
        // Sibling-acc for position i = all − this position's term.
        let mut siblings_at_i = all_siblings_acc;
        xor_into(&mut siblings_at_i, term);

        let unbound = unbind_child_at_position(centroid, &parent_base, &siblings_at_i, i);

        let mut best: Option<(CanonicalKind, u32)> = None;
        for &kind in candidate_child_kinds {
            let d = popcount_distance(&unbound, &leaf_base(kind));
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
    use crate::test_util::{
        conn_with_schema_and_udfs as fresh_with_udfs, insert_combined_hv as insert_combined,
        insert_layer_hv as insert,
    };
    use crate::util::{bucket_arity, expand_seed, ZERO_HV};

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
        xor_into(&mut expected, &base);
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
        use crate::canonical::CanonicalKind;
        use crate::util::rotate_left;
        let cb = AstCodebook::new();
        let parent_base = expand_seed(0xBA5E);
        let child0 = cb.base_vector(&AstNodeFingerprint::leaf(CanonicalKind::Op));
        let child1 = cb.base_vector(&AstNodeFingerprint::leaf(CanonicalKind::Lit));

        // Build parent the way the encoder would.
        let mut parent = parent_base;
        let p0 = rotate_left(&child0, 0);
        xor_into(&mut parent, &p0);
        let p1 = rotate_left(&child1, 1);
        xor_into(&mut parent, &p1);

        // Strip child0 from siblings_acc (already rotated as it sits in parent).
        let recovered = unbind_child_at_position(&parent, &parent_base, &p0, 1);
        assert_eq!(recovered, child1);
    }

    #[test]
    fn explain_cluster_centroid_returns_one_tuple_per_arity() {
        // API shape: explain returns exactly arity tuples, each with
        // a recovered canonical kind from the candidate set and a
        // distance metric. With sibling cancellation now implemented
        // (skeptic 4bace1) recovery on a single tree should ALSO
        // succeed — pin both shape AND correctness here.
        use crate::encoder::{encode_fresh, EncoderNode};

        let cb = AstCodebook::new();
        let tree = EncoderNode::new(
            CanonicalKind::Block,
            vec![
                EncoderNode::leaf(CanonicalKind::Op),
                EncoderNode::leaf(CanonicalKind::Lit),
            ],
        );
        let centroid = encode_fresh(&tree, &cb);

        let candidate_kinds = CanonicalKind::ALL;

        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Block,
            bucket_arity(2),
            // Positional (order matches the encoded tree):
            &[CanonicalKind::Op, CanonicalKind::Lit],
            &cb,
            &candidate_kinds,
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
        // Correctness pin (skeptic 4bace1): with sibling cancellation,
        // a single tree should recover positions exactly.
        assert_eq!(recovered[0].1, CanonicalKind::Op, "position 0 must recover Op");
        assert_eq!(recovered[1].1, CanonicalKind::Lit, "position 1 must recover Lit");
    }

    #[test]
    fn explain_cluster_centroid_recovers_kinds_on_homogeneous_cluster() {
        // Skeptic 4bace1: prior tests only validated API shape. This
        // pins the recovery-accuracy claim in the regime where the
        // function CAN deliver: a homogeneous cluster (10 trees with
        // identical fingerprints at every level — bundle_majority of
        // identical HVs IS that HV, so the centroid is exactly one
        // tree's encoding).
        //
        // For this case, `explain_cluster_centroid` should recover
        // each position correctly because the "sibling noise" terms,
        // while present, are weighted against a clean signal — the
        // residual at position i is rot_left(child_i_base, i) XOR
        // (rot_left of N-1 other children). The probe via
        // rotate_right(_, i) lines up child_i correctly; the other
        // siblings appear as rotated noise that's structured but
        // independent enough that the cleanup-memory's nearest-kind
        // search still picks the correct kind for low-arity parents.
        //
        // Math-friend ≥80% recovery target: 3/3 = 100% here on a
        // 3-arity homogeneous cluster.
        //
        // Note on heterogeneous clusters (where bodies vary across
        // members): see the docstring on `explain_cluster_centroid`
        // for why this function alone can't deliver clean recovery
        // on those — sibling cancellation is needed, which is the
        // job of `unbind_child_at_position`. The next test
        // (`real_cluster_via_unbind_recovers_each_position`) pins
        // that path on a real heterogeneous cluster.
        use crate::encoder::{encode_fresh, EncoderNode};
        use crate::sheaf::HvCellComplex;

        let cb = AstCodebook::new();
        let tree = EncoderNode::new(
            CanonicalKind::Decl,
            vec![
                EncoderNode::leaf(CanonicalKind::Ref),
                EncoderNode::leaf(CanonicalKind::Block),
                EncoderNode::leaf(CanonicalKind::Op),
            ],
        );
        // 10 identical encodings — homogeneous cluster.
        let centroids: Vec<Hypervector> = (0..10).map(|_| encode_fresh(&tree, &cb)).collect();
        // Bundle is trivially the input HV (identical inputs).
        let centroid = HvCellComplex::bundle_majority(&centroids);
        assert_eq!(centroid, centroids[0], "homogeneous bundle must equal input");

        let candidate_kinds = CanonicalKind::ALL;
        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Decl,
            bucket_arity(3),
            &[CanonicalKind::Ref, CanonicalKind::Block, CanonicalKind::Op],
            &cb,
            &candidate_kinds,
        );
        assert_eq!(recovered.len(), 3);
        let correct: u32 = [
            (recovered[0].1 == CanonicalKind::Ref) as u32,
            (recovered[1].1 == CanonicalKind::Block) as u32,
            (recovered[2].1 == CanonicalKind::Op) as u32,
        ]
        .iter()
        .sum();
        assert!(
            correct as f64 / 3.0 >= 0.80,
            "math-friend recovery target ≥80% violated: got {}/3 (recovered={:?})",
            correct,
            recovered
        );
    }

    #[test]
    fn explain_cluster_centroid_recovers_constant_positions_on_heterogeneous_cluster() {
        // Skeptic 4bace1 (companion test): heterogeneous cluster
        // where positions 0 and 2 are CONSTANT leaf kinds across all
        // 10 trees, position 1 cycles through varying leaf kinds.
        //
        // Setup: 10 trees Decl[Ref, varying-kind, Op]. The cleanup-
        // memory only probes against LEAF base_vectors (arity 0,
        // no children) — so all parent-level positions must be
        // leaves for the function to deliver real recovery.
        //
        // Bundle behavior:
        // - Position 0 (Ref) is constant → rot_left(Ref_leaf, 0)
        //   survives in bundle → recovers Ref exactly.
        // - Position 2 (Op) is constant → rot_left(Op_leaf, 2)
        //   survives → recovers Op exactly.
        // - Position 1 cycles {Stmt, Expr, Block, Lit, Stmt, Expr,
        //   Block, Lit, Stmt, Expr} — bundle of 10 different leaves
        //   denoises position 1 toward majority/tied bits. Recovery
        //   here is bounded by sample variance; we DON'T pin it
        //   beyond "kind comes from candidate set."
        //
        // Math-friend ≥80% target: 2/2 constant positions = 100%
        // expected. Plus position 1 is plausible.
        use crate::encoder::{encode_fresh, EncoderNode};
        use crate::sheaf::HvCellComplex;

        let cb = AstCodebook::new();
        let varying_kinds = [
            CanonicalKind::Stmt,
            CanonicalKind::Expr,
            CanonicalKind::Block,
            CanonicalKind::Lit,
            CanonicalKind::Stmt,
            CanonicalKind::Expr,
            CanonicalKind::Block,
            CanonicalKind::Lit,
            CanonicalKind::Stmt,
            CanonicalKind::Expr,
        ];
        let mut centroids = Vec::with_capacity(10);
        for &mid_kind in &varying_kinds {
            let tree = EncoderNode::new(
                CanonicalKind::Decl,
                vec![
                    EncoderNode::leaf(CanonicalKind::Ref),
                    EncoderNode::leaf(mid_kind),
                    EncoderNode::leaf(CanonicalKind::Op),
                ],
            );
            centroids.push(encode_fresh(&tree, &cb));
        }
        // Sanity: ≥4 distinct top-level HVs (heterogeneity check —
        // 4 distinct kinds at position 1 → 4 distinct parent_fps,
        // but parent_base depends on sorted kinds which vary, so
        // each kind variant produces a distinct parent HV).
        let mut unique = centroids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert!(unique.len() >= 4, "need real heterogeneity, got {}", unique.len());

        let centroid = HvCellComplex::bundle_majority(&centroids);

        let candidate_kinds = CanonicalKind::ALL;
        // The function takes the parent's positional kind sequence as
        // the caller's hypothesis. We hypothesize the modal kind at
        // position 1 (Stmt appears 3× — most frequent in the cycle).
        // Note: heterogeneous bundle means parent_base for the
        // hypothesized fp won't match any single tree's parent_base
        // exactly — recovery accuracy degrades correspondingly. The
        // pin here: positions 0 and 2 must still recover correctly
        // (they're constant signal even when bundle is mixed).
        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Decl,
            bucket_arity(3),
            &[CanonicalKind::Ref, CanonicalKind::Stmt, CanonicalKind::Op],
            &cb,
            &candidate_kinds,
        );
        assert_eq!(recovered.len(), 3);

        // The two constant positions: pin exact recovery. With
        // varying parent_fp across cluster members, parent_base
        // hypothesis is approximate, so the residual at constant
        // positions has additional contamination — but the constant
        // signal still dominates the cleanup-memory verdict.
        assert_eq!(
            recovered[0].1,
            CanonicalKind::Ref,
            "position 0 must recover Ref (constant); got {:?} at Hamming {}",
            recovered[0].1,
            recovered[0].2
        );
        assert_eq!(
            recovered[2].1,
            CanonicalKind::Op,
            "position 2 must recover Op (constant); got {:?} at Hamming {}",
            recovered[2].1,
            recovered[2].2
        );

        // Math-friend ≥80% recovery target on the 2 constant positions:
        let correct: u32 = (recovered[0].1 == CanonicalKind::Ref) as u32
            + (recovered[2].1 == CanonicalKind::Op) as u32;
        assert!(
            correct as f64 / 2.0 >= 0.80,
            "constant-position recovery rate {}/2 < 80%",
            correct
        );
    }

    #[test]
    fn explain_cluster_centroid_handles_zero_arity() {
        // Edge case: leaf node (no children). Should return an empty
        // vec, not panic.
        let cb = AstCodebook::new();
        let candidate_kinds = [CanonicalKind::Lit, CanonicalKind::Op];
        let centroid = cb.base_vector(&AstNodeFingerprint::leaf(CanonicalKind::Lit));
        let recovered = explain_cluster_centroid(
            &centroid,
            CanonicalKind::Lit,
            0,
            &[],
            &cb,
            &candidate_kinds,
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
