//! SHA-256 Merkle tree with domain separation.
//!
//! Provides generic Merkle root computation and leaf hashing, used by
//! both the sheaf cache (structural invalidation) and the network layer
//! (manifest verification).
//!
//! Domain separation tags:
//! - `0x00` — leaf node
//! - `0x01` — internal node

use sha2::{Digest, Sha256};

/// Compute a Merkle root from a set of leaf hashes.
///
/// Uses SHA-256 with domain separation:
/// - Leaf nodes: `H(0x00 || leaf_data)`
/// - Internal nodes: `H(0x01 || left || right)`
///
/// If the number of leaves is odd, the last leaf is promoted without hashing.
/// An empty input returns all zeros.
pub fn compute_merkle_root(leaf_hashes: &[[u8; 32]]) -> [u8; 32] {
    if leaf_hashes.is_empty() {
        return [0u8; 32];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_root_empty() {
        assert_eq!(compute_merkle_root(&[]), [0u8; 32]);
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
}
