//! **F4 — Editing one leaf in an n-leaf Merkle tree causes at most
//! ⌈log₂(n)⌉ + ε rehashes.**
//!
//! Falsifies substrate requirement R6 (O(log n) subtree Merkle
//! propagation) — decade
//! `docs/decades/2026-merkle-cas-substrate.md` §4 F4.
//!
//! ## Claim (from the decade §4)
//!
//! > "F4 — Diff complexity. Construct Merkle tree over n = 50,000
//! > documents. Edit k = 1 leaf. Count hashes recomputed. Predicted:
//! > `O(log_2 n) ≈ 17`. Falsified if recomputation is ≥ √n or n."
//!
//! ## Test shape
//!
//! ll-core does not yet ship a Merkle tree (that's the T4 thread's
//! work — see decade §5). Per the bead's guidance, the test builds a
//! minimal binary Merkle tree in-file using the substrate's chosen
//! hash primitive (BLAKE3, locked per Σ §3.4). All hash calls go
//! through a per-tree `HashCounter` (an `Arc<AtomicU64>` field on the
//! tree), so parallel tests don't step on each other and the count is
//! exact.
//!
//! The tree is padded up to the next power of two (65,536 leaves for
//! n=50,000) — required for a perfect binary layout. Editing a leaf
//! at position ~n/2 walks the tree upward, recomputing exactly one
//! parent at each level. Total: 1 leaf rehash + log₂(padded_n)
//! internal-node rehashes.
//!
//! For padded_n = 65,536 that's log₂(65,536) = 16 internal levels + 1
//! leaf hash = 17 hashes. Bound = 19 (ε = 2 slack per decade §4).
//!
//! ## Pass criteria
//!
//! - Build produces exactly `N_LEAVES + 1 + (n_padded - 1)` hash calls
//!   (real leaves + sentinel-precompute + internal nodes). Pins the
//!   harness — if build cost were wrong, the edit-cost measurement
//!   would be unreliable.
//! - Edit re-hashes ≤ 19 nodes (⌈log₂(50000)⌉ ≈ 16 + 1 leaf + ε=2).
//! - Root changes after the edit (sanity: the edit actually propagated).
//! - Editing at extremes (idx=0 and idx=n_padded-1) matches the same
//!   bound (catches off-by-one sibling errors).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Total leaves in the Merkle tree. Matches decade §4 F4 spec exactly.
const N_LEAVES: usize = 50_000;

/// Position of the leaf to edit. Middle-ish so we exercise a full
/// log₂(n) path both ways.
const EDIT_INDEX: usize = 25_000;

/// Number of edit iterations. Each iteration measures the hash count
/// for a single leaf edit; all must stay within bound.
const EDIT_ITERATIONS: usize = 2;

/// ε (epsilon) slack allowed above ⌈log₂(n_padded)⌉ + 1 to account for
/// hash-bookkeeping calls at the boundary (e.g. the leaf's own re-hash
/// counted separately from the internal-node walk). Decade §4 F4 spec.
const HASH_COUNT_EPSILON: u64 = 2;

/// Per-tree hash counter. Each `MerkleTree` owns its own `Arc<AtomicU64>`
/// so parallel tests running in the same binary don't step on each
/// other's counts. Cheap to clone — the tree closes over it and every
/// `parent_hash` call increments through the same handle.
type HashCounter = Arc<AtomicU64>;

/// Wrapper around `blake3::hash` that increments the passed counter.
/// Direct `blake3::hash` calls in tests are auto-allowed by the
/// `lint:blake3` gate (Taskfile lint gate auto-allows `tests/`).
fn counting_hash(counter: &HashCounter, bytes: &[u8]) -> [u8; 32] {
    counter.fetch_add(1, Ordering::Relaxed);
    *blake3::hash(bytes).as_bytes()
}

/// Parent = σ(left ‖ right). Matches the Merkle structure in the
/// decade doc §1.3 (`σ(c) = σ(σ(c_1) ‖ σ(c_2) ‖ … ‖ σ(c_k))`, k=2 here).
fn parent_hash(counter: &HashCounter, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    counting_hash(counter, &buf)
}

/// Minimal in-file Merkle tree for F4. Perfect binary layout: leaves
/// are padded up to the next power of two by re-hashing an empty-blob
/// sentinel, so every internal node has two children and the update
/// walk is a clean O(log n).
///
/// The tree is NOT part of ll-core's shipped surface — it's a test
/// fixture built specifically to falsify R6. When the T4 thread ships
/// a real Merkle tree in the substrate, F4 should be re-pointed at
/// that impl (and this fixture deleted).
struct MerkleTree {
    counter: HashCounter,
    /// `levels[0]` = leaf hashes, `levels[levels.len()-1]` = single
    /// root hash. `levels[k][i]` is the i-th node at level k.
    levels: Vec<Vec<[u8; 32]>>,
}

impl MerkleTree {
    /// Build a Merkle tree over `leaves`. Pads with hash-of-empty to
    /// the next power of two.
    ///
    /// Cost: `N_LEAVES` leaf hashes + 1 padding-sentinel precompute +
    /// (n_padded - 1) internal-node hashes.
    fn build(counter: HashCounter, leaves: &[Vec<u8>]) -> Self {
        // Pad up to next power of two.
        let mut n_padded = 1usize;
        while n_padded < leaves.len() {
            n_padded *= 2;
        }

        // Level 0: hash every leaf blob into a fixed-size hash.
        let mut leaf_hashes: Vec<[u8; 32]> =
            leaves.iter().map(|b| counting_hash(&counter, b)).collect();
        // Pad with hash-of-empty. Precompute once, then clone into the
        // tail — otherwise the padding walk would inflate the hash
        // count by n_padded - n extra calls and the build-cost pin
        // would need to shift. This is a fixture choice: pad-sentinel
        // is off-count for slot fill because it's a byte copy of a
        // known value.
        let empty_hash = counting_hash(&counter, &[]);
        while leaf_hashes.len() < n_padded {
            leaf_hashes.push(empty_hash);
        }

        let mut levels = vec![leaf_hashes];
        while levels.last().unwrap().len() > 1 {
            let cur = levels.last().unwrap();
            let next: Vec<[u8; 32]> = cur
                .chunks(2)
                .map(|pair| parent_hash(&counter, &pair[0], &pair[1]))
                .collect();
            levels.push(next);
        }
        Self { counter, levels }
    }

    fn root(&self) -> [u8; 32] {
        *self.levels.last().unwrap().first().unwrap()
    }

    /// Number of levels in the tree (`log₂(n_padded) + 1`).
    fn depth(&self) -> usize {
        self.levels.len()
    }

    /// Number of padded leaves in level 0.
    fn n_padded(&self) -> usize {
        self.levels[0].len()
    }

    /// Update the leaf at `idx` to hash `new_leaf_bytes`, walking up
    /// and re-hashing every ancestor. Returns the new root.
    ///
    /// Cost: 1 leaf hash + `depth - 1` internal-node hashes =
    /// `depth = log₂(n_padded) + 1` total. For n_padded=65,536: 17.
    fn update_leaf(&mut self, mut idx: usize, new_leaf_bytes: &[u8]) -> [u8; 32] {
        assert!(
            idx < self.n_padded(),
            "F4 harness bug: update_leaf idx {idx} >= n_padded {}",
            self.n_padded()
        );
        // Recompute leaf hash.
        self.levels[0][idx] = counting_hash(&self.counter, new_leaf_bytes);

        // Walk upward: at each level, the parent index is `idx / 2`,
        // the sibling index is `idx ^ 1`. The chunks-of-2 ordering in
        // `build` puts pair (idx & !1, idx | 1) together.
        let n_internal_levels = self.levels.len() - 1;
        for level in 0..n_internal_levels {
            let sibling = idx ^ 1;
            let (left, right) = if idx & 1 == 0 {
                (self.levels[level][idx], self.levels[level][sibling])
            } else {
                (self.levels[level][sibling], self.levels[level][idx])
            };
            let parent_idx = idx / 2;
            self.levels[level + 1][parent_idx] = parent_hash(&self.counter, &left, &right);
            idx = parent_idx;
        }
        self.root()
    }
}

/// ⌈log₂(x)⌉ for x >= 1.
fn ceil_log2(x: usize) -> u32 {
    assert!(x >= 1, "F4 harness bug: ceil_log2({x}) undefined");
    if x == 1 {
        return 0;
    }
    (x - 1).ilog2() + 1
}

fn make_leaves(prefix: &str) -> Vec<Vec<u8>> {
    (0..N_LEAVES)
        .map(|i| format!("{prefix}-{i}").into_bytes())
        .collect()
}

#[test]
fn build_hash_count_matches_n_plus_1_plus_n_padded_minus_1() {
    // Precondition sanity for the F4 measurement: `build` costs
    // exactly `N_LEAVES + 1 + (n_padded - 1)` hashes. If a future
    // refactor changes the build cost, the edit-cost measurement's
    // baseline shifts and the main F4 test's assertion loses meaning.
    // Pin the build cost separately so a build-cost regression is
    // diagnosed independently.
    let counter: HashCounter = Arc::new(AtomicU64::new(0));
    let leaves = make_leaves("f4-build");

    let tree = MerkleTree::build(counter.clone(), &leaves);
    let build_calls = counter.load(Ordering::Relaxed);

    let n_padded = tree.n_padded();
    // Cost breakdown:
    //   - N_LEAVES real-leaf hashes (one per input blob)
    //   - 1 padding-sentinel precompute (hash of the empty slice)
    //   - (n_padded - 1) internal-node hashes to reduce level 0 up to
    //     the root
    let expected = (N_LEAVES as u64) + 1 + (n_padded as u64 - 1);
    assert_eq!(
        build_calls, expected,
        "F4 build-cost pin: expected {expected} hash calls \
         (n_leaves={N_LEAVES}, n_padded={n_padded}), got {build_calls}. \
         Change indicates the tree's build shape changed — re-audit \
         the F4 edit-cost assertion before adjusting.",
    );

    // Sanity: the tree's depth reflects the padded-power-of-two shape.
    assert_eq!(
        tree.depth(),
        (ceil_log2(n_padded) + 1) as usize,
        "F4 harness bug: depth ({}) ≠ log₂(n_padded)+1",
        tree.depth()
    );
}

#[test]
fn single_leaf_edit_rehashes_at_most_log2_n_padded_plus_epsilon() {
    let counter: HashCounter = Arc::new(AtomicU64::new(0));
    let leaves = make_leaves("f4-edit");
    let mut tree = MerkleTree::build(counter.clone(), &leaves);
    let n_padded = tree.n_padded();
    let bound = (ceil_log2(n_padded) as u64) + 1 + HASH_COUNT_EPSILON;

    // Sanity: n_padded for n=50,000 is 65,536; log₂ = 16; bound = 19.
    // (16 + 1 + 2). Decade §4 predicts "≈ 17", well inside bound.
    assert_eq!(n_padded, 65_536, "F4 sanity: n_padded for n=50000 is 65536");
    assert_eq!(
        bound, 19,
        "F4 sanity: bound = log₂(65536) + 1 + ε(2) = 16+1+2 = 19"
    );

    let root_before = tree.root();

    for iter in 0..EDIT_ITERATIONS {
        counter.store(0, Ordering::Relaxed);
        let new_bytes = format!("f4-edit-iter-{iter}").into_bytes();
        let new_root = tree.update_leaf(EDIT_INDEX, &new_bytes);
        let edit_calls = counter.load(Ordering::Relaxed);

        // Sanity: root changed (edit actually propagated).
        if iter == 0 {
            assert_ne!(
                new_root, root_before,
                "F4 harness bug: edit did not change root (leaf update did not propagate)"
            );
        }

        assert!(
            edit_calls <= bound,
            "F4 falsified: single-leaf edit at idx {EDIT_INDEX} required {edit_calls} \
             hashes on iteration {iter} (bound = {bound} = ⌈log₂({n_padded})⌉+1+ε({HASH_COUNT_EPSILON})). \
             R6 (O(log n) subtree Merkle propagation) is broken.",
        );

        // Also assert the recomputation didn't degrade to √n or n
        // (decade §4's explicit "falsified if …" threshold).
        let sqrt_n = (N_LEAVES as f64).sqrt() as u64;
        assert!(
            edit_calls < sqrt_n,
            "F4 catastrophically falsified: edit cost {edit_calls} ≥ √n ({sqrt_n})",
        );
    }
}

#[test]
fn edit_at_extremes_matches_log_bound() {
    // Edit at index 0 and index n_padded-1: both walks must stay
    // within the same log₂ bound. A tree implementation with off-by-
    // one in the sibling calculation would surface here (one edge
    // would pay double).
    let counter: HashCounter = Arc::new(AtomicU64::new(0));
    let leaves = make_leaves("f4-extremes");
    let mut tree = MerkleTree::build(counter.clone(), &leaves);
    let n_padded = tree.n_padded();
    let bound = (ceil_log2(n_padded) as u64) + 1 + HASH_COUNT_EPSILON;

    for idx in [0usize, n_padded - 1] {
        counter.store(0, Ordering::Relaxed);
        let _ = tree.update_leaf(idx, &format!("edited-at-{idx}").into_bytes());
        let calls = counter.load(Ordering::Relaxed);
        assert!(
            calls <= bound,
            "F4 extremes: edit at idx {idx} = {calls} hashes (bound {bound})"
        );
    }
}
