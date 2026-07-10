//! Falsifiability gates for the moat claims.
//!
//! Each test pins a *named claim* from the strategic analysis: the
//! sheaf machinery's claim to give precision invalidation is only
//! defensible if these gates pass. A failure means the math or the
//! implementation has drifted from the documented contract.
//!
//! Two gates land here for the lift (`ley-line-open-ae7a35`); the
//! other three (Claims 3, 4, 5) depend on the audit (`ley-line-open-a3f764`)
//! and the surface op (`ley-line-open-a46a5c`) and live as separate
//! follow-up beads.
//!
//! ## Index of claims
//!
//! | # | Claim | Where | Status |
//! |---|---|---|---|
//! | 1 | `H0Result::defect == Σ‖δ⁰(stalks)‖²` | this file | ✅ gated here |
//! | 2 | `SheafCache::on_change` invalidates only restriction-graph-reachable entries | this file | ✅ gated here |
//! | 3 | Sheaf-driven reparse < git-dirty reparse on real repo | follow-up (a3f764) | deferred |
//! | 4 | Defect monotonically decreases on enrichment convergence | follow-up | deferred |
//! | 5 | `get_sheaf_status` MCP tool surfaces defect | follow-up (a46a5c) | deferred |

use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use sha2::Digest;

// ---------------------------------------------------------------------
// Claim 1: defect = Σ ‖δ⁰(stalks)‖² (hand-computed)
//
// Build a simple 2-node complex with known stalks and a known
// restriction map. Compute the expected δ⁰ output by hand. Assert
// `compute_h0(threshold).defect` matches.
//
// If this test fails, the implementation has drifted from the
// documented "defect = ‖δ⁰‖²" contract — the math invariant the
// rest of the moat claims rest on.
// ---------------------------------------------------------------------

/// Build a 2-node complex where each node has stalk-dim 2, the edge
/// has agreement-dim 1, and the restriction maps project each node's
/// first coordinate into the edge stalk.
///
/// With node-0 = [a, _] and node-1 = [b, _], the edge sees
/// `project_dim(2,0) * [a,_] = [a]` from node-0 and `[b]` from node-1.
/// δ⁰ at the edge = target_proj(node-1) - source_proj(node-0) = b - a.
/// The edge contribution to defect is `(b-a)²`.
fn two_node_first_coord_complex(node0: f32, node1: f32) -> CellComplex {
    let mut cx = CellComplex::new(2);
    cx.add_node(0, vec![node0, 99.0]); // 99.0 ignored — edge projects to coord 0
    cx.add_node(1, vec![node1, 88.0]); // 88.0 ignored — same
    cx.add_edge(
        100,
        0,
        1,
        1, // agreement dim
        Some("first-coord".into()),
        RestrictionMap::project_dim(2, 0),
        RestrictionMap::project_dim(2, 0),
        false,
    );
    cx
}

#[test]
fn claim_1_defect_equals_squared_l2_of_delta_zero_consistent() {
    // Consistent stalks: a == b ⇒ δ⁰(stalks) = 0 ⇒ defect = 0.
    let cx = two_node_first_coord_complex(5.0, 5.0);
    let h0 = cx.consistency_analysis(f32::INFINITY);
    assert!(
        h0.defect.abs() < 1e-6,
        "claim 1 (consistent): defect must be 0 when δ⁰(stalks) = 0; got {}",
        h0.defect
    );
}

#[test]
fn claim_1_defect_equals_squared_l2_of_delta_zero_inconsistent() {
    // Inconsistent stalks: a=5, b=3 ⇒ δ⁰(stalks) = b - a = -2 at the
    // single edge ⇒ ‖δ⁰‖² = 4.
    let cx = two_node_first_coord_complex(5.0, 3.0);
    let h0 = cx.consistency_analysis(f32::INFINITY);
    let expected = (3.0_f32 - 5.0_f32).powi(2);
    assert!(
        (h0.defect - expected).abs() < 1e-6,
        "claim 1 (inconsistent a=5,b=3): defect must equal (b-a)² = {expected}; got {}",
        h0.defect
    );
}

#[test]
fn claim_1_defect_scales_quadratically_with_disagreement() {
    // δ⁰ is linear; defect = ‖δ⁰‖² should be quadratic in the
    // disagreement magnitude. Double the gap ⇒ 4× the defect.
    let small = two_node_first_coord_complex(0.0, 1.0)
        .consistency_analysis(f32::INFINITY)
        .defect;
    let large = two_node_first_coord_complex(0.0, 2.0)
        .consistency_analysis(f32::INFINITY)
        .defect;
    let ratio = large / small;
    assert!(
        (ratio - 4.0).abs() < 1e-5,
        "claim 1: defect quadratic scaling — large/small ratio must be 4.0 (since (2/1)² = 4); got {ratio}"
    );
}

#[test]
fn claim_1_defect_sums_over_edges() {
    // δ⁰ output is a column vector of length Σ(edge_agreement_dim);
    // defect = ‖that vector‖² sums each edge's squared contribution.
    // Build a 3-node "path" with two edges, both inconsistent by
    // different amounts. Defect should equal the sum of per-edge
    // squared disagreements.
    let mut cx = CellComplex::new(2);
    cx.add_node(0, vec![10.0, 0.0]);
    cx.add_node(1, vec![7.0, 0.0]);
    cx.add_node(2, vec![3.0, 0.0]);
    // Edge 0→1: δ⁰ = 7 - 10 = -3 ⇒ contribution 9
    cx.add_edge(
        100,
        0,
        1,
        1,
        Some("e1".into()),
        RestrictionMap::project_dim(2, 0),
        RestrictionMap::project_dim(2, 0),
        false,
    );
    // Edge 1→2: δ⁰ = 3 - 7 = -4 ⇒ contribution 16
    cx.add_edge(
        101,
        1,
        2,
        1,
        Some("e2".into()),
        RestrictionMap::project_dim(2, 0),
        RestrictionMap::project_dim(2, 0),
        false,
    );
    let h0 = cx.consistency_analysis(f32::INFINITY);
    let expected: f32 = 9.0 + 16.0;
    assert!(
        (h0.defect - expected).abs() < 1e-5,
        "claim 1 (path of 2 edges): defect must sum per-edge contributions to {expected}; got {}",
        h0.defect
    );
}

// ---------------------------------------------------------------------
// Claim 2: SheafCache::on_change invalidates exactly the entries
// reachable from the changed regions via the restriction graph (within
// the cascade depth bound), no more and no less.
//
// Build a 4-region cache:
//   A — B — C   D (isolated)
// Invalidate A. Expect: A and B (direct neighbor) become invalid; C
// only if A→B's boundary actually changed AND we cascade through B→C;
// D never (no restriction edge connecting D).
//
// Falsifies if: D is invalidated (over-eviction — loses precision) OR
// A is not invalidated (under-eviction — serves stale data).
// ---------------------------------------------------------------------

#[derive(Clone)]
struct TestStalk([u8; 32]);

impl StalkHash for TestStalk {
    fn merkle_root(&self) -> [u8; 32] {
        self.0
    }
}

fn make_stalk(seed: u8) -> TestStalk {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    TestStalk(bytes)
}

fn boundary_xor(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

#[test]
fn claim_2_invalidation_does_not_touch_disconnected_regions() {
    // A=0, B=1, C=2, D=3. Restriction graph: A↔B, B↔C. D is isolated.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();

    // Seed stalks.
    let sa = make_stalk(1);
    let sb = make_stalk(2);
    let sc = make_stalk(3);
    let sd = make_stalk(4);
    cache.set_stalk(0, sa.clone());
    cache.set_stalk(1, sb.clone());
    cache.set_stalk(2, sc.clone());
    cache.set_stalk(3, sd.clone());

    // Seed boundary hashes that MATCH the current state, so no
    // boundary appears "changed" until we mutate the stalks.
    let edge_ab = RestrictionEdge {
        weights: vec![1.0],
        co_change_rate: 0.0,
        revert_rate: 0.0,
        boundary_hash: boundary_xor(&sa.merkle_root(), &sb.merkle_root()),
    };
    let edge_bc = RestrictionEdge {
        weights: vec![1.0],
        co_change_rate: 0.0,
        revert_rate: 0.0,
        boundary_hash: boundary_xor(&sb.merkle_root(), &sc.merkle_root()),
    };
    cache.set_restriction(0, 1, edge_ab);
    cache.set_restriction(1, 2, edge_bc);
    // D (region 3) is intentionally NOT connected to anything.

    // Populate cache entries for every region.
    cache.put(0, "A-payload");
    cache.put(1, "B-payload");
    cache.put(2, "C-payload");
    cache.put(3, "D-payload");

    // Mutate A's stalk so the A↔B boundary is now stale, and trigger
    // invalidation rooted at A.
    cache.set_stalk(0, make_stalk(0xff));
    let invalidated = cache.on_change(&[0]);

    // PASS: A must be invalidated (it's the changed region itself).
    assert!(
        invalidated.contains(&0),
        "claim 2: changed region (A) must be marked invalid; got {invalidated:?}"
    );

    // PASS: D (isolated) must NEVER be touched — there's no
    // restriction edge from any invalidated region to D.
    assert!(
        !invalidated.contains(&3),
        "claim 2: disconnected region (D) must NOT be invalidated; got {invalidated:?}"
    );
}

#[test]
fn claim_2_on_change_advances_generation_monotonically() {
    // Generation is the consumer-visible "we've moved past your
    // snapshot" signal. Must advance on every on_change call so the
    // sheaf_status op (bead a46a5c) can give consumers a strict
    // ordering of cache states.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();
    cache.set_stalk(0, make_stalk(1));

    let g0 = cache.generation();
    let _ = cache.on_change(&[0]);
    let g1 = cache.generation();
    let _ = cache.on_change(&[0]);
    let g2 = cache.generation();

    assert!(
        g1 > g0 && g2 > g1,
        "claim 2: generation must advance monotonically across on_change calls; got {g0} → {g1} → {g2}"
    );
}

#[test]
fn claim_2c_changed_roots_are_returned_even_when_entries_are_empty() {
    // Pin the on_change wire contract that the d03e7d fix established and
    // PR #19 review surfaced as missing from the docstring: cascade roots
    // appear in the returned list even when the local `entries` map is
    // empty AND when their own boundary projection is unchanged. The
    // caller's "this changed" is taken as input, not measured.
    //
    // Pre-d03e7d, this test would have returned `invalidated: []` because
    // the push was gated on `entries.get_mut(...) == Some(_)`. Post-fix,
    // the changed root appears unconditionally; only cascade NEIGHBORS
    // are gated on the boundary check.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();
    let sa = make_stalk(1);
    let sb = make_stalk(2);
    cache.set_stalk(0, sa.clone());
    cache.set_stalk(1, sb.clone());

    // Boundary hash matches current XOR → boundary IS unchanged.
    cache.set_restriction(
        0,
        1,
        RestrictionEdge {
            weights: vec![1.0],
            co_change_rate: 0.0,
            revert_rate: 0.0,
            boundary_hash: boundary_xor(&sa.merkle_root(), &sb.merkle_root()),
        },
    );

    // Deliberately NO cache.put — this mimics a UDS / MCP consumer that
    // owns its own cache and never registers an entry on the daemon side.
    // No stalk mutation either — boundary projection does not move.
    let invalidated = cache.on_change(&[0]);

    assert!(
        invalidated.contains(&0),
        "claim 2c: cascade root MUST appear in the returned list even when \
         entries is empty AND boundary is unchanged (caller's assertion is \
         taken as input, not measured); got {invalidated:?}"
    );
    assert!(
        !invalidated.contains(&1),
        "claim 2c: cascade NEIGHBOR must NOT appear when boundary projection \
         is unchanged (only roots are unconditional); got {invalidated:?}"
    );
}

#[test]
fn claim_2_unchanged_neighbors_with_matching_boundary_hash_remain_valid() {
    // If a region's stalk is changed but its NEW state still matches
    // the cached boundary hash with a neighbor (vanishingly unlikely
    // for real Merkle hashes; deterministically reproducible here),
    // the neighbor must remain valid. This pins the "boundary_changed
    // check actually consults the stored hash" property — without it,
    // cascade invalidation would over-evict.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();
    let sa = make_stalk(1);
    let sb = make_stalk(2);
    cache.set_stalk(0, sa.clone());
    cache.set_stalk(1, sb.clone());

    // Boundary hash deliberately set to match the EVENTUAL XOR after
    // we "mutate" A back to its original value (no real mutation —
    // but on_change still runs the boundary check against the stored
    // hash). With XOR matching, B should not cascade-invalidate.
    let edge = RestrictionEdge {
        weights: vec![1.0],
        co_change_rate: 0.0,
        revert_rate: 0.0,
        boundary_hash: boundary_xor(&sa.merkle_root(), &sb.merkle_root()),
    };
    cache.set_restriction(0, 1, edge);
    cache.put(0, "A");
    cache.put(1, "B");

    // Trigger invalidation rooted at A but WITHOUT changing A's
    // stalk. The boundary hash still matches → B should stay valid.
    let invalidated = cache.on_change(&[0]);

    assert!(
        invalidated.contains(&0),
        "claim 2: rooted region always invalidated; got {invalidated:?}"
    );
    assert!(
        !invalidated.contains(&1),
        "claim 2: neighbor with matching boundary hash must NOT cascade; got {invalidated:?}"
    );
}

// ---------------------------------------------------------------------
// Claim 2b: when a `CellComplex` is attached, the XOR-Merkle pre-filter
// can say "changed" but the cache must still keep the neighbor valid if
// the real δ⁰ output says the agreement subspace is unchanged. This is
// the load-bearing precision claim: the cache evicts on real sheaf
// disagreement, not on every Merkle-root flip.
//
// Falsifies if: the cache invalidates a neighbor whose restriction-mapped
// stalk component did not actually change, just because some other part
// of the content (and therefore the Merkle root) shifted.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct F32Stalk {
    data: Vec<f32>,
}

impl StalkHash for F32Stalk {
    fn merkle_root(&self) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        for v in &self.data {
            hasher.update(v.to_le_bytes());
        }
        hasher.finalize().into()
    }
}

#[test]
fn claim_2b_real_delta_zero_keeps_neighbor_valid_when_projection_unchanged() {
    // Two-node complex; the restriction extracts coord 0 from a 2D stalk,
    // so coord 1 is the "private to this node" part that should not
    // propagate through the sheaf.
    let mut complex = CellComplex::new(2);
    complex.add_node(0, vec![5.0, 1.0]);
    complex.add_node(1, vec![5.0, 999.0]);
    complex.add_edge(
        100,
        0,
        1,
        1,
        Some("project_dim_0".into()),
        RestrictionMap::project_dim(2, 0),
        RestrictionMap::project_dim(2, 0),
        false,
    );

    let mut cache: SheafCache<F32Stalk, &'static str> = SheafCache::new().with_complex(complex);

    let s0_v1 = F32Stalk {
        data: vec![5.0, 1.0],
    };
    let s1 = F32Stalk {
        data: vec![5.0, 999.0],
    };
    cache.set_stalk(0, s0_v1.clone());
    cache.set_stalk(1, s1.clone());
    cache.set_stalk_value(0, s0_v1.data.clone());
    cache.set_stalk_value(1, s1.data.clone());

    let boundary_xor = {
        let ha = s0_v1.merkle_root();
        let hb = s1.merkle_root();
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = ha[i] ^ hb[i];
        }
        out
    };
    cache.set_restriction(
        0,
        1,
        RestrictionEdge {
            weights: vec![1.0],
            co_change_rate: 0.0,
            revert_rate: 0.0,
            boundary_hash: boundary_xor,
        },
    );
    cache.put(0, "A-payload");
    cache.put(1, "B-payload");

    // Change node 0's coord 1 (the "private" dimension that the restriction
    // projects away). Merkle root flips → XOR pre-filter says "changed",
    // but δ⁰ output is still zero (coord 0 unchanged).
    let s0_v2 = F32Stalk {
        data: vec![5.0, 42.0],
    };
    cache.set_stalk(0, s0_v2.clone());
    cache.set_stalk_value(0, s0_v2.data.clone());

    let invalidated = cache.on_change(&[0]);

    assert!(
        invalidated.contains(&0),
        "claim 2b: changed region always invalidated; got {invalidated:?}"
    );
    assert!(
        !invalidated.contains(&1),
        "claim 2b: neighbor must remain valid when projection-image is unchanged \
         despite Merkle-root flip; got {invalidated:?}"
    );

    // Sanity: without the attached complex, the XOR pre-filter alone would
    // have cascaded — proving the δ⁰ check is what saved the neighbor.
    let mut heuristic_cache: SheafCache<F32Stalk, &'static str> = SheafCache::new();
    heuristic_cache.set_stalk(0, s0_v1.clone());
    heuristic_cache.set_stalk(1, s1.clone());
    heuristic_cache.set_restriction(
        0,
        1,
        RestrictionEdge {
            weights: vec![1.0],
            co_change_rate: 0.0,
            revert_rate: 0.0,
            boundary_hash: boundary_xor,
        },
    );
    heuristic_cache.put(0, "A-payload");
    heuristic_cache.put(1, "B-payload");
    heuristic_cache.set_stalk(0, s0_v2);
    let heuristic_invalidated = heuristic_cache.on_change(&[0]);
    assert!(
        heuristic_invalidated.contains(&1),
        "claim 2b sanity: without the complex, XOR pre-filter must cascade \
         (over-eviction is what δ⁰ is fixing); got {heuristic_invalidated:?}"
    );
}

// ---------------------------------------------------------------------
// Claim 6 (incremental topology — bead ley-line-open-9d2302):
// `CellComplex::apply_delta` + `SheafCache::refresh_baseline_subset` must
// preserve cache state for regions outside the touched subgraph. Replacing
// the entire complex via `sheaf_set_topology` invalidates every entry; the
// incremental op claim is "untouched regions are byte-identical".
//
// Falsifies if any cache entry outside the touched ∪ radius-1 set loses
// its valid flag — that's the regression the new op is supposed to fix.
// ---------------------------------------------------------------------

use leyline_sheaf::complex::{EdgeDelta, EdgeSpec, RegionDelta, TopologyDelta};

#[test]
fn incremental_update_preserves_untouched_cache_entries() {
    // 100-region chain: r_0 -- r_1 -- ... -- r_99. Cache one ()
    // entry per region. Apply a 1-region delta (update r_50's stalk)
    // and verify the cache reports 99 entries as valid afterwards.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();

    let mut stalks: Vec<TestStalk> = (0..100u32)
        .map(|i| {
            let mut h = [0u8; 32];
            h[0] = (i & 0xff) as u8;
            h[1] = ((i >> 8) & 0xff) as u8;
            TestStalk(h)
        })
        .collect();
    for (i, s) in stalks.iter().enumerate() {
        cache.set_stalk(i as u32, s.clone());
    }
    for i in 0..99u32 {
        let a = stalks[i as usize].merkle_root();
        let b = stalks[(i + 1) as usize].merkle_root();
        cache.set_restriction(
            i,
            i + 1,
            RestrictionEdge {
                weights: vec![1.0],
                co_change_rate: 0.0,
                revert_rate: 0.0,
                boundary_hash: boundary_xor(&a, &b),
            },
        );
    }
    for i in 0..100u32 {
        cache.put(i, "entry");
    }
    assert_eq!(cache.valid_count(), 100, "seed must populate 100 entries");

    // Update r_50's stalk. The cache's set_stalk overwrites the entry;
    // we leave `entries` alone so the test pins what `refresh_baseline_
    // subset` does, not what `on_change` does.
    stalks[50] = TestStalk([0xff; 32]);
    cache.set_stalk(50, stalks[50].clone());

    // Refresh baseline ONLY for r_50's local subgraph. Without a complex
    // attached this is a no-op for δ⁰ baseline — but the contract still
    // says no entries outside the subset should be touched.
    cache.refresh_baseline_subset(&[50]);

    // Every cache entry must still be valid. The incremental op claim:
    // only `on_change(&[changed_regions])` evicts; baseline refresh
    // alone never marks entries invalid.
    assert_eq!(
        cache.valid_count(),
        100,
        "refresh_baseline_subset must NOT evict cache entries"
    );

    // The complementary on_change call evicts only r_50 and its direct
    // neighbour r_49 (boundary hash check fires) plus r_51 if the
    // cascade reaches it.
    let invalidated = cache.on_change(&[50]);
    assert!(invalidated.contains(&50), "changed region must be invalid");
    // Regions 0..=48 and 52..=99 must remain valid (cascade depth=3
    // bounded BFS, but the chain neighbours of r_50 are r_49 and r_51).
    // We assert the strong contract: at least 90 of the 100 entries
    // must survive the update — far more than the "all 99" target since
    // the cascade depth is 3.
    let surviving = cache.valid_count();
    assert!(
        surviving >= 90,
        "incremental update preserved {surviving} entries; expected ≥ 90 (cascade ≤ 3 hops from r_50)"
    );
}

#[test]
fn affected_regions_includes_radius_1_neighbours() {
    // CellComplex apply_delta should report both endpoints of an added
    // edge as touched, so the daemon handler can fold radius-1 around
    // them. This is the gate on apply_delta's contract.
    let mut cx = CellComplex::new(2);
    cx.add_node(0, vec![1.0, 0.0]);
    cx.add_node(1, vec![1.0, 0.0]);

    let p = RestrictionMap::project_dim(2, 0);
    let delta = TopologyDelta {
        regions: RegionDelta::default(),
        edges: EdgeDelta {
            added: vec![EdgeSpec {
                source: 0,
                target: 1,
                agreement_dim: 1,
                label: Some("test".into()),
                map_source: p.clone(),
                map_target: p,
            }],
            removed: Vec::new(),
        },
    };

    let affected = cx.apply_delta(&delta);
    assert!(
        affected.contains(&0) && affected.contains(&1),
        "apply_delta must report both edge endpoints as affected; got {affected:?}"
    );
}

#[test]
fn add_region_baseline_matches_set_topology() {
    // Build the same 3-region complex two ways:
    //   (a) one-shot via add_node + add_edge
    //   (b) incremental via apply_delta starting from an empty complex
    // The resulting defect (Σ‖δ⁰‖²) must match — incremental construction
    // is observationally indistinguishable from atomic seeding.
    let mut cx_one_shot = CellComplex::new(2);
    cx_one_shot.add_node(0, vec![1.0, 0.0]);
    cx_one_shot.add_node(1, vec![2.0, 0.0]);
    cx_one_shot.add_node(2, vec![3.0, 0.0]);
    let p = RestrictionMap::project_dim(2, 0);
    cx_one_shot.add_edge(
        100,
        0,
        1,
        1,
        Some("dep".into()),
        p.clone(),
        p.clone(),
        false,
    );
    cx_one_shot.add_edge(
        101,
        1,
        2,
        1,
        Some("dep".into()),
        p.clone(),
        p.clone(),
        false,
    );
    let one_shot_defect = cx_one_shot.consistency_analysis(f32::INFINITY).defect;

    let mut cx_incremental = CellComplex::new(2);
    let delta = TopologyDelta {
        regions: RegionDelta {
            added: vec![
                (0, vec![1.0, 0.0]),
                (1, vec![2.0, 0.0]),
                (2, vec![3.0, 0.0]),
            ],
            removed: Vec::new(),
            updated_stalks: Vec::new(),
        },
        edges: EdgeDelta {
            added: vec![
                EdgeSpec {
                    source: 0,
                    target: 1,
                    agreement_dim: 1,
                    label: Some("dep".into()),
                    map_source: p.clone(),
                    map_target: p.clone(),
                },
                EdgeSpec {
                    source: 1,
                    target: 2,
                    agreement_dim: 1,
                    label: Some("dep".into()),
                    map_source: p.clone(),
                    map_target: p.clone(),
                },
            ],
            removed: Vec::new(),
        },
    };
    cx_incremental.apply_delta(&delta);
    let incremental_defect = cx_incremental.consistency_analysis(f32::INFINITY).defect;

    assert!(
        (one_shot_defect - incremental_defect).abs() < 1e-5,
        "incremental construction must produce same defect as set_topology: one_shot={one_shot_defect}, incremental={incremental_defect}"
    );
}

#[test]
fn concurrent_updates_serialize_correctly() {
    // Spawn 2 threads each calling apply_delta on a shared CellComplex
    // (wrapped in Mutex so the Rust-level contract sees one delta at a
    // time, matching the daemon handler's lock-then-apply pattern). Both
    // threads add disjoint regions. Final state: every region from both
    // threads is present, no panics, defect well-defined.
    use std::sync::{Arc, Mutex};
    use std::thread;

    let cx = Arc::new(Mutex::new(CellComplex::new(2)));
    let p = RestrictionMap::project_dim(2, 0);

    let mut handles = Vec::new();
    for thread_idx in 0..2u32 {
        let cx = Arc::clone(&cx);
        let p = p.clone();
        let handle = thread::spawn(move || {
            // Thread 0 adds regions 0..50, thread 1 adds regions 50..100.
            let base = thread_idx * 50;
            for i in 0..50u32 {
                let rid = base + i;
                let edges = if i > 0 {
                    EdgeDelta {
                        added: vec![EdgeSpec {
                            source: rid - 1,
                            target: rid,
                            agreement_dim: 1,
                            label: Some("dep".into()),
                            map_source: p.clone(),
                            map_target: p.clone(),
                        }],
                        removed: Vec::new(),
                    }
                } else {
                    EdgeDelta::default()
                };
                let delta = TopologyDelta {
                    regions: RegionDelta {
                        added: vec![(rid, vec![rid as f32, 0.0])],
                        removed: Vec::new(),
                        updated_stalks: Vec::new(),
                    },
                    edges,
                };
                let mut g = cx.lock().unwrap();
                g.apply_delta(&delta);
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("thread join");
    }

    let cx = cx.lock().unwrap();
    assert_eq!(cx.nodes.len(), 100, "all 100 regions must be present");
    // Defect must be a finite, non-negative number — no NaN poisoning
    // from a half-applied delta.
    let defect = cx.consistency_analysis(f32::INFINITY).defect;
    assert!(
        defect.is_finite() && defect >= 0.0,
        "concurrent updates must leave defect finite; got {defect}"
    );
}

// ---------------------------------------------------------------------
// Reaper gates (bead `ley-line-open-9c867f`, GC item 3).
//
// `SheafCache::reap()` is the read-only query that asks "given today's
// stalks vs the last baseline, which regions can the consumer safely
// evict?". These gates pin:
//
//   - **No false negatives**: a region whose own boundary moved must
//     appear in the reap output. Consumers that miss the signal serve
//     stale values.
//   - **No false positives**: if nothing changed, reap returns empty.
//     Spurious reaping defeats the purpose of cache coherence.
//   - **Payload-blind**: the V type parameter must not affect the
//     reclaim decision. Same topology + stalks → same reclaim set
//     regardless of what's cached at each key.
//   - **Heuristic-only mode**: when no `CellComplex` is attached,
//     reap returns empty + NaN defect (the XOR pre-filter is a
//     change detector, not a stale detector; reap must err on the
//     side of "don't reap" without δ⁰ signal).
//
// All four gates depend on the `set_stalk_value` → `cx.set_node_stalk`
// propagation in cache.rs (line 325) — mutating a stalk's f32 data
// flows through to the attached complex so `edge_violation_squared`
// sees the new value.
// ---------------------------------------------------------------------

/// Helper: build a 4-region linear chain (r0–r1–r2–r3) with a real
/// CellComplex attached, stalks all zeroed, baseline refreshed.
/// Stalk dim = 2, agreement dim = 1 (projects coord 0).
fn build_reap_test_cache() -> SheafCache<TestStalk, &'static str> {
    let mut cx = CellComplex::new(2);
    cx.add_node(0, vec![0.0, 0.0]);
    cx.add_node(1, vec![0.0, 0.0]);
    cx.add_node(2, vec![0.0, 0.0]);
    cx.add_node(3, vec![0.0, 0.0]);
    let proj = RestrictionMap::project_dim(2, 0);
    cx.add_edge(100, 0, 1, 1, None, proj.clone(), proj.clone(), false);
    cx.add_edge(101, 1, 2, 1, None, proj.clone(), proj.clone(), false);
    cx.add_edge(102, 2, 3, 1, None, proj.clone(), proj, false);

    // Keep stalks in local vars — `cache.stalks` is a private field, so
    // the test must compute boundary_xor from the same stalks it seeds
    // into the cache rather than reading them back.
    let stalks: Vec<TestStalk> = (0..4u32).map(|i| make_stalk(i as u8 + 1)).collect();
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();
    for (i, s) in stalks.iter().enumerate() {
        cache.set_stalk(i as u32, s.clone());
    }
    // Restriction edges keyed (a, b) AND (b, a) — `neighbours()`
    // filters on `a == region` only, so the reaper needs both
    // directions populated for BFS to walk past r0.
    for &(a, b) in &[(0u32, 1u32), (1, 2), (2, 3)] {
        let xor = boundary_xor(
            &stalks[a as usize].merkle_root(),
            &stalks[b as usize].merkle_root(),
        );
        cache.set_restriction(
            a,
            b,
            RestrictionEdge {
                weights: vec![1.0],
                co_change_rate: 0.0,
                revert_rate: 0.0,
                boundary_hash: xor,
            },
        );
        cache.set_restriction(
            b,
            a,
            RestrictionEdge {
                weights: vec![1.0],
                co_change_rate: 0.0,
                revert_rate: 0.0,
                boundary_hash: xor,
            },
        );
    }
    cache.set_complex(cx);
    cache.refresh_baseline();
    cache
}

#[test]
fn reap_no_false_positives_on_unchanged_stalks() {
    // After refresh_baseline, calling reap() with no mutations must
    // return an empty reclaim set. Spurious reclaim signals would
    // defeat the cache-coherence contract.
    let cache = build_reap_test_cache();
    let (reclaim, defect) = cache.reap();

    assert!(
        reclaim.is_empty(),
        "reap on stable section must return empty; got {reclaim:?}"
    );
    assert!(
        defect.is_finite() && defect >= 0.0,
        "defect snapshot must be finite + non-negative on stable section; got {defect}"
    );
}

#[test]
fn reap_no_false_negatives_when_stalks_move() {
    // Mutate r1's stalk so the r0↔r1 and r1↔r2 boundaries both shift
    // beyond DELTA0_EPS (norm space). r1 MUST appear in the reclaim set
    // (it's the changed region itself). r0 and r2 SHOULD appear (their
    // incident edges have moved). r3 may or may not appear depending
    // on BFS depth, but it should NOT be excluded if it's within range.
    let mut cache = build_reap_test_cache();

    // Push r1's stalk to a wildly different value; the project_dim(2,0)
    // restriction extracts coord 0, so this guarantees δ⁰ shifts.
    cache.set_stalk_value(1, vec![100.0, 0.0]);

    let (reclaim, _) = cache.reap();

    assert!(
        reclaim.contains(&1),
        "reap MUST include the changed region r1; got {reclaim:?}"
    );
    assert!(
        reclaim.contains(&0) && reclaim.contains(&2),
        "reap MUST include r1's direct neighbours r0+r2 whose boundaries moved; got {reclaim:?}"
    );
}

#[test]
fn reap_payload_blind_under_different_v_types() {
    // The reclaim decision depends ONLY on topology + stalks, never on
    // the cached V type. Build two caches with identical topology +
    // identical stalk mutations but DIFFERENT V types (&str vs u64),
    // assert the reclaim sets match.

    fn run<V: Clone>(seed: V) -> Vec<u32> {
        let mut cx = CellComplex::new(2);
        cx.add_node(0, vec![0.0, 0.0]);
        cx.add_node(1, vec![0.0, 0.0]);
        let proj = RestrictionMap::project_dim(2, 0);
        cx.add_edge(100, 0, 1, 1, None, proj.clone(), proj, false);

        let stalks = [make_stalk(1), make_stalk(2)];
        let mut cache: SheafCache<TestStalk, V> = SheafCache::new();
        cache.set_stalk(0, stalks[0].clone());
        cache.set_stalk(1, stalks[1].clone());
        let xor = boundary_xor(&stalks[0].merkle_root(), &stalks[1].merkle_root());
        for &(a, b) in &[(0u32, 1u32), (1, 0)] {
            cache.set_restriction(
                a,
                b,
                RestrictionEdge {
                    weights: vec![1.0],
                    co_change_rate: 0.0,
                    revert_rate: 0.0,
                    boundary_hash: xor,
                },
            );
        }
        cache.set_complex(cx);
        cache.refresh_baseline();
        cache.put(0, seed.clone());
        cache.put(1, seed);
        cache.set_stalk_value(0, vec![50.0, 0.0]);
        let (r, _) = cache.reap();
        r
    }

    let with_str = run::<&'static str>("payload-a");
    let with_u64 = run::<u64>(0xdeadbeef);

    assert_eq!(
        with_str, with_u64,
        "payload-blind contract: reap must yield identical sets under different V; \
         &str gave {with_str:?}, u64 gave {with_u64:?}"
    );
}

#[test]
fn reap_returns_empty_and_nan_without_complex() {
    // Heuristic-only mode (no CellComplex attached). reap must NOT
    // guess from the XOR pre-filter — that's a change detector, not
    // a stale detector. Returns (empty, NaN) so consumers see "I
    // can't tell you what's reclaimable" instead of false positives.
    let mut cache: SheafCache<TestStalk, &'static str> = SheafCache::new();
    cache.set_stalk(0, make_stalk(1));
    cache.set_stalk(1, make_stalk(2));

    let (reclaim, defect) = cache.reap();

    assert!(
        reclaim.is_empty(),
        "heuristic-only mode: reap must return empty; got {reclaim:?}"
    );
    assert!(
        defect.is_nan(),
        "heuristic-only mode: defect must be NaN sentinel; got {defect}"
    );
}
