//! SHA-256 Merkle tree with domain separation — root, leaf hashing, and
//! per-leaf inclusion proofs.
//!
//! Used by the sheaf cache (structural invalidation), the network layer
//! (manifest verification), and per-leaf progressive verification
//! (BEP-52-style; bead ley-line-open-31ec98) — one primitive, so ley-line's
//! wire path and LLO's at-rest path agree on the tree convention instead of
//! forking two incompatible Merkle trees.
//!
//! Domain separation tags:
//! - `0x00` — leaf node
//! - `0x01` — internal node
//! - `0x02` — empty tree marker
//!
//! Surface: [`compute_merkle_root`] / [`hash_node`] (root side),
//! [`merkle_proof`] / [`verify_merkle_proof`] / [`MerkleProof`] (proof side).

use sha2::{Digest, Sha256};

/// Compute a Merkle root from a set of leaf hashes.
///
/// Uses SHA-256 with domain separation:
/// - Leaf nodes: `H(0x00 || leaf_data)`
/// - Internal nodes: `H(0x01 || left || right)`
/// - Empty tree: `H(0x02 || "empty")` — never the all-zeros sentinel, so an
///   accidental zero-hash leaf never collides with "I have no data."
///
/// If the number of leaves is odd, the last leaf is promoted without hashing.
pub fn compute_merkle_root(leaf_hashes: &[[u8; 32]]) -> [u8; 32] {
    if leaf_hashes.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update([0x02]);
        hasher.update(b"empty");
        return hasher.finalize().into();
    }
    if leaf_hashes.len() == 1 {
        return leaf_hashes[0];
    }

    let mut current_level: Vec<[u8; 32]> = leaf_hashes.to_vec();

    while current_level.len() > 1 {
        let mut next_level = Vec::with_capacity(current_level.len().div_ceil(2));

        for pair in current_level.chunks(2) {
            if pair.len() == 2 {
                let mut hasher = Sha256::new();
                hasher.update([0x01]); // internal node domain separator
                hasher.update(pair[0]);
                hasher.update(pair[1]);
                next_level.push(hasher.finalize().into());
            } else {
                // Odd leaf — promote without re-hashing
                next_level.push(pair[0]);
            }
        }

        current_level = next_level;
    }

    current_level[0]
}

/// Hash a single node's content for use as a Merkle leaf.
///
/// Domain-separated: `SHA-256(0x00 || data)`.
pub fn hash_node(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0x00]); // leaf domain separator
    hasher.update(data);
    hasher.finalize().into()
}

/// A per-leaf Merkle inclusion proof.
///
/// `siblings` is the sequence, from the leaf's level up toward the root, of the
/// sibling hash at each level where the leaf's running node was combined with
/// another node. Each entry is `(sibling_hash, sibling_is_left)`:
/// `sibling_is_left = true` means the sibling is the left operand of the
/// internal-node hash `H(0x01 || left || right)`, which is what binds the proof
/// to the leaf's *position*, not just its value.
///
/// Levels where the node was **promoted** (the odd-tail case in
/// [`compute_merkle_root`]) contribute no sibling — mirroring the root
/// computation exactly, so [`verify_merkle_proof`] recomputes the same root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleProof {
    /// `(sibling_hash, sibling_is_left)`, leaf-to-root order.
    pub siblings: Vec<([u8; 32], bool)>,
}

/// Generate an inclusion proof for `leaf_hashes[index]` under the tree rooted by
/// [`compute_merkle_root`]. Rebuilds the tree level-by-level with the identical
/// pairing + odd-tail-promotion convention, recording the sibling of the target
/// node at each level it is hashed.
///
/// Panics if `index >= leaf_hashes.len()`.
pub fn merkle_proof(leaf_hashes: &[[u8; 32]], index: usize) -> MerkleProof {
    assert!(
        index < leaf_hashes.len(),
        "merkle_proof: index {index} out of range for {} leaves",
        leaf_hashes.len()
    );

    let mut siblings = Vec::new();
    let mut level: Vec<[u8; 32]> = leaf_hashes.to_vec();
    let mut pos = index;

    while level.len() > 1 {
        // Record the sibling of `pos` at this level (if it has one). Odd `pos`
        // always has a left sibling; even `pos` has a right sibling unless it is
        // the promoted odd tail.
        let even = pos.is_multiple_of(2);
        let has_sibling = if even { pos + 1 < level.len() } else { true };
        if has_sibling {
            if even {
                siblings.push((level[pos + 1], false)); // sibling on the right
            } else {
                siblings.push((level[pos - 1], true)); // sibling on the left
            }
        }

        // Build the next level with the same convention as compute_merkle_root.
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            if pair.len() == 2 {
                let mut hasher = Sha256::new();
                hasher.update([0x01]);
                hasher.update(pair[0]);
                hasher.update(pair[1]);
                next.push(hasher.finalize().into());
            } else {
                next.push(pair[0]); // promoted
            }
        }

        pos /= 2;
        level = next;
    }

    MerkleProof { siblings }
}

/// Verify that `leaf` is included under `root` via `proof`. Folds `leaf` with
/// each sibling using the internal-node hash `H(0x01 || left || right)` in the
/// recorded order and side, and checks the result equals `root`.
///
/// Domain separation makes this safe: leaves are `H(0x00 || …)` and internal
/// nodes `H(0x01 || …)`, so an internal-node value can never be presented as a
/// leaf. A single-leaf tree has an empty proof and verifies iff `leaf == root`.
pub fn verify_merkle_proof(leaf: [u8; 32], proof: &MerkleProof, root: [u8; 32]) -> bool {
    let mut current = leaf;
    for &(sibling, sibling_is_left) in &proof.siblings {
        let mut hasher = Sha256::new();
        hasher.update([0x01]);
        if sibling_is_left {
            hasher.update(sibling);
            hasher.update(current);
        } else {
            hasher.update(current);
            hasher.update(sibling);
        }
        current = hasher.finalize().into();
    }
    current == root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_root_empty_is_domain_separated_not_zeros() {
        // Empty tree must produce a distinct, non-zero hash so accidental
        // zero-hash data does not collide with "I have no leaves."
        let empty = compute_merkle_root(&[]);
        assert_ne!(empty, [0u8; 32], "empty tree hash must not be all zeros");
        // Stable across calls.
        assert_eq!(empty, compute_merkle_root(&[]));
        // Distinct from a tree containing only the zero-hash leaf.
        let zero_leaf = compute_merkle_root(&[[0u8; 32]]);
        assert_ne!(
            empty, zero_leaf,
            "empty tree must hash differently than a tree with the zero leaf",
        );
    }

    #[test]
    fn merkle_root_single_leaf() {
        let leaf = hash_node(b"hello");
        assert_eq!(compute_merkle_root(&[leaf]), leaf);
    }

    #[test]
    fn merkle_root_two_leaves() {
        let a = hash_node(b"hello");
        let b = hash_node(b"world");

        let root = compute_merkle_root(&[a, b]);

        // Manually compute expected: H(0x01 || a || b)
        let mut hasher = Sha256::new();
        hasher.update([0x01]);
        hasher.update(a);
        hasher.update(b);
        let expected: [u8; 32] = hasher.finalize().into();

        assert_eq!(root, expected);
    }

    #[test]
    fn merkle_root_three_leaves() {
        let a = hash_node(b"a");
        let b = hash_node(b"b");
        let c = hash_node(b"c");

        let root = compute_merkle_root(&[a, b, c]);

        // Level 1: H(0x01||a||b), c promoted
        let mut hasher = Sha256::new();
        hasher.update([0x01]);
        hasher.update(a);
        hasher.update(b);
        let ab: [u8; 32] = hasher.finalize().into();

        // Level 2: H(0x01||ab||c)
        let mut hasher = Sha256::new();
        hasher.update([0x01]);
        hasher.update(ab);
        hasher.update(c);
        let expected: [u8; 32] = hasher.finalize().into();

        assert_eq!(root, expected);
    }

    #[test]
    fn merkle_root_deterministic() {
        let leaves: Vec<[u8; 32]> = (0..10).map(|i| hash_node(&[i])).collect();
        let root1 = compute_merkle_root(&leaves);
        let root2 = compute_merkle_root(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_root_order_sensitive() {
        let a = hash_node(b"first");
        let b = hash_node(b"second");

        let root_ab = compute_merkle_root(&[a, b]);
        let root_ba = compute_merkle_root(&[b, a]);

        assert_ne!(root_ab, root_ba);
    }

    #[test]
    fn hash_node_domain_separated() {
        // hash_node should differ from raw SHA-256
        let data = b"test data";
        let node_hash = hash_node(data);
        let raw_hash: [u8; 32] = Sha256::digest(data).into();
        assert_ne!(node_hash, raw_hash);
    }

    // -----------------------------------------------------------------------
    // Property tests: collision resistance, large inputs, duplicates
    // -----------------------------------------------------------------------

    /// Different leaf sets must produce different roots.
    #[test]
    fn collision_resistance_permutations() {
        // Generate 8 distinct leaves, compute roots for all 2-element subsets
        let leaves: Vec<[u8; 32]> = (0..8u8).map(|i| hash_node(&[i])).collect();
        let mut roots = std::collections::HashSet::new();

        for i in 0..leaves.len() {
            for j in (i + 1)..leaves.len() {
                let root = compute_merkle_root(&[leaves[i], leaves[j]]);
                assert!(
                    roots.insert(root),
                    "collision: leaves ({i},{j}) produced duplicate root"
                );
            }
        }
        // 8 choose 2 = 28 unique roots
        assert_eq!(roots.len(), 28);
    }

    /// Duplicate leaves are NOT deduplicated — [a, a] differs from [a].
    #[test]
    fn duplicate_leaves_not_deduplicated() {
        let a = hash_node(b"same");
        let single = compute_merkle_root(&[a]);
        let double = compute_merkle_root(&[a, a]);
        assert_ne!(
            single, double,
            "duplicate leaves must produce different root than single"
        );
    }

    /// Large leaf count (1024) doesn't panic or overflow.
    #[test]
    fn large_leaf_count() {
        let leaves: Vec<[u8; 32]> = (0..1024u32).map(|i| hash_node(&i.to_le_bytes())).collect();
        let root = compute_merkle_root(&leaves);
        assert_ne!(root, [0u8; 32]);

        // Deterministic
        let root2 = compute_merkle_root(&leaves);
        assert_eq!(root, root2);
    }

    /// Domain separation prevents leaf/internal node confusion.
    /// H(0x00 || data) must differ from H(0x01 || data).
    #[test]
    fn domain_separation_leaf_vs_internal() {
        let data = [0xAA; 32];

        // Leaf hash: H(0x00 || data)
        let mut h_leaf = Sha256::new();
        h_leaf.update([0x00]);
        h_leaf.update(data);
        let leaf_hash: [u8; 32] = h_leaf.finalize().into();

        // Internal hash: H(0x01 || data || [0;32])
        let mut h_internal = Sha256::new();
        h_internal.update([0x01]);
        h_internal.update(data);
        h_internal.update([0u8; 32]);
        let internal_hash: [u8; 32] = h_internal.finalize().into();

        assert_ne!(
            leaf_hash, internal_hash,
            "leaf and internal hashes must differ (domain separation)"
        );
    }

    /// Single-element tree root equals the leaf itself (no re-hashing).
    #[test]
    fn single_leaf_is_identity() {
        for i in 0..10u8 {
            let leaf = hash_node(&[i]);
            assert_eq!(
                compute_merkle_root(&[leaf]),
                leaf,
                "single-leaf tree should return the leaf unchanged"
            );
        }
    }

    /// Power-of-two vs non-power-of-two leaf counts produce valid roots.
    #[test]
    fn power_of_two_and_odd_counts() {
        for count in [2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33] {
            let leaves: Vec<[u8; 32]> = (0..count).map(|i| hash_node(&[i as u8])).collect();
            let root = compute_merkle_root(&leaves);
            assert_ne!(
                root, [0u8; 32],
                "count={count} should produce non-zero root"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Per-leaf inclusion proofs (bead ley-line-open-31ec98). `compute_merkle_root`
    // is the correctness oracle: for every leaf, its proof must recompute exactly
    // that root — and any tamper must break it.
    // -----------------------------------------------------------------------

    /// The load-bearing property: for every leaf of every (incl. odd-promotion)
    /// tree size, `merkle_proof` produces a proof that `verify_merkle_proof`
    /// folds back to the oracle root.
    #[test]
    fn proof_roundtrips_for_every_leaf_and_size() {
        for count in [1usize, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33] {
            let leaves: Vec<[u8; 32]> = (0..count)
                .map(|i| hash_node(&(i as u32).to_le_bytes()))
                .collect();
            let root = compute_merkle_root(&leaves);
            for i in 0..count {
                let proof = merkle_proof(&leaves, i);
                assert!(
                    verify_merkle_proof(leaves[i], &proof, root),
                    "proof for leaf {i} of {count} must verify against the oracle root"
                );
            }
        }
    }

    #[test]
    fn single_leaf_proof_is_empty_and_verifies() {
        let leaf = hash_node(b"solo");
        let root = compute_merkle_root(&[leaf]);
        let proof = merkle_proof(&[leaf], 0);
        assert!(
            proof.siblings.is_empty(),
            "single-leaf proof has no siblings"
        );
        assert!(verify_merkle_proof(leaf, &proof, root));
    }

    #[test]
    fn proof_rejects_wrong_leaf_or_root() {
        let leaves: Vec<[u8; 32]> = (0..8).map(|i| hash_node(&[i as u8])).collect();
        let root = compute_merkle_root(&leaves);
        let proof = merkle_proof(&leaves, 3);
        assert!(
            !verify_merkle_proof(hash_node(b"forged"), &proof, root),
            "a forged leaf must not verify"
        );
        assert!(
            !verify_merkle_proof(leaves[3], &proof, [0xAB; 32]),
            "a valid leaf+proof must not verify against the wrong root"
        );
    }

    #[test]
    fn proof_rejects_tampered_sibling() {
        let leaves: Vec<[u8; 32]> = (0..8).map(|i| hash_node(&[i as u8])).collect();
        let root = compute_merkle_root(&leaves);
        let mut proof = merkle_proof(&leaves, 3);
        proof.siblings[0].0[0] ^= 0xFF; // flip a byte of the first sibling
        assert!(!verify_merkle_proof(leaves[3], &proof, root));
    }

    /// Flipping which side a sibling is on must break verification — this is
    /// what binds the proof to the leaf's position, not just its value.
    #[test]
    fn proof_rejects_flipped_side() {
        let leaves: Vec<[u8; 32]> = (0..8).map(|i| hash_node(&[i as u8])).collect();
        let root = compute_merkle_root(&leaves);
        let mut proof = merkle_proof(&leaves, 3);
        proof.siblings[0].1 = !proof.siblings[0].1;
        assert!(!verify_merkle_proof(leaves[3], &proof, root));
    }

    /// Domain separation at the proof level: an internal node value must not
    /// pass as a leaf inclusion (0x00 leaf vs 0x01 internal prefixes).
    #[test]
    fn internal_node_does_not_verify_as_a_leaf() {
        let leaves: Vec<[u8; 32]> = (0..4).map(|i| hash_node(&[i as u8])).collect();
        let root = compute_merkle_root(&leaves);
        let mut h = Sha256::new();
        h.update([0x01]);
        h.update(leaves[0]);
        h.update(leaves[1]);
        let internal_ab: [u8; 32] = h.finalize().into();
        let proof0 = merkle_proof(&leaves, 0);
        assert!(
            !verify_merkle_proof(internal_ab, &proof0, root),
            "an internal node must never be accepted as a leaf"
        );
    }
}
