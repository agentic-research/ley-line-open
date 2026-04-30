//! HDC encoder: walks an `EncoderNode` tree post-order, produces a
//! function-level hypervector. Uses a content-hash subtree cache so
//! identical subtrees aren't re-encoded.
//!
//! Parser-agnostic by design — the cli-lib daemon adapter (in a future
//! `daemon::hdc_pass` module) walks tree-sitter and builds `EncoderNode`
//! trees; this crate doesn't depend on tree-sitter directly. Same
//! encoder works for any structural tree.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::canonical::CanonicalKind;
use crate::codebook::{AstNodeFingerprint, BaseCodebook};
use crate::util::{rotate_left, xor_into, Hypervector};

/// One node in a tree to encode. Owns its children so the encoder can
/// recurse without borrow-lifetime gymnastics. Built from tree-sitter
/// (or any other parser) by the cli-lib daemon adapter.
#[derive(Debug, Clone)]
pub struct EncoderNode {
    pub canonical_kind: CanonicalKind,
    /// Sorted list of child canonical kinds — matches what the codebook
    /// expects in `AstNodeFingerprint`. Order-invariant at the codebook
    /// level; child position re-encoded via role-binding in the encoder.
    pub child_canonical_kinds_sorted: Vec<CanonicalKind>,
    /// Children in their original (parser-given) order. Encoder XOR-binds
    /// each child's hypervector with `role_vector(i)` to encode position.
    pub children: Vec<EncoderNode>,
}

impl EncoderNode {
    /// Convenience constructor.
    pub fn new(kind: CanonicalKind, children: Vec<EncoderNode>) -> Self {
        let mut sorted: Vec<CanonicalKind> = children.iter().map(|c| c.canonical_kind).collect();
        sorted.sort_unstable_by_key(|k| k.discriminant());
        EncoderNode {
            canonical_kind: kind,
            child_canonical_kinds_sorted: sorted,
            children,
        }
    }

    /// Convenience constructor for a leaf node (no children).
    /// Equivalent to `EncoderNode::new(kind, vec![])`. Eliminates
    /// the `leaf(kind)` helper that several test modules duplicated.
    pub fn leaf(kind: CanonicalKind) -> Self {
        Self::new(kind, vec![])
    }

    /// Build the codebook fingerprint for this node.
    fn fingerprint(&self) -> AstNodeFingerprint {
        AstNodeFingerprint {
            canonical_kind: self.canonical_kind,
            arity_bucket: crate::util::bucket_arity(self.children.len()),
            child_canonical_kinds: self.child_canonical_kinds_sorted.clone(),
        }
    }

    /// Content hash for cache lookup. Deterministic across machines —
    /// covers the kind, arity, sorted child kinds, AND each child's
    /// content hash, so structurally-identical subtrees collapse to
    /// the same hash regardless of where they appear in a tree.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.canonical_kind.discriminant()]);
        hasher.update(&[crate::util::bucket_arity(self.children.len())]);
        for k in &self.child_canonical_kinds_sorted {
            hasher.update(&[k.discriminant()]);
        }
        for child in &self.children {
            hasher.update(&child.content_hash());
        }
        *hasher.finalize().as_bytes()
    }
}

/// Codebook-tagged cache key. Same `node` under two different codebooks
/// (e.g. AstCodebook and ModuleCodebook) produces two distinct keys,
/// so a shared cache cannot return one codebook's entries when queried
/// by another. Tag prefix is hashed alongside the content hash.
fn cache_key(node: &EncoderNode, codebook_tag: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(codebook_tag.as_bytes());
    hasher.update(b"\0"); // separator
    hasher.update(&node.content_hash());
    *hasher.finalize().as_bytes()
}

/// Content-hash → hypervector cache. Identical subtrees encode once;
/// structurally-equivalent subtrees in different files re-use the same
/// hypervector. Critical for bidi recovery — without the cache, the
/// bind algebra is one-way (per math-friend review G).
///
/// Thread-safe so the encoder can be invoked from any tokio task.
pub struct SubtreeCache {
    map: Mutex<HashMap<[u8; 32], Hypervector>>,
}

impl SubtreeCache {
    pub fn new() -> Self {
        SubtreeCache {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<Hypervector> {
        self.map.lock().unwrap().get(key).copied()
    }

    pub fn put(&self, key: [u8; 32], hv: Hypervector) {
        self.map.lock().unwrap().insert(key, hv);
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SubtreeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a tree of `EncoderNode` post-order into a single hypervector.
///
/// Algorithm (per math-friend review):
/// 1. Compute content_hash; if cache-hit, return cached HV.
/// 2. Recursively encode each child.
/// 3. Start with the codebook's base_vector for this node.
/// 4. XOR each child's HV with `role_vector(i)` and into the accumulator.
/// 5. Cache the result by content_hash.
///
/// This is hierarchical bind+bundle (Plate 1994 / Schlegel 2022) —
/// avoids the saturation ceiling on flat bundles by encoding tree shape
/// into nested role bindings rather than into one giant bundle.
pub fn encode_tree<C>(node: &EncoderNode, codebook: &C, cache: &SubtreeCache) -> Hypervector
where
    C: BaseCodebook<Item = AstNodeFingerprint>,
{
    // Cache key mixes the codebook's tag into the content-hash so
    // the same SubtreeCache can safely be shared across codebooks
    // without one codebook's entries leaking back to another.
    // Skeptic-review (bead 4ba0cf) caught the silent cross-poisoning
    // bug; this closes it without changing the cache's API.
    let key = cache_key(node, codebook.codebook_tag());
    if let Some(cached) = cache.get(&key) {
        return cached;
    }

    let fp = node.fingerprint();
    let mut hv = codebook.base_vector(&fp);

    for (i, child) in node.children.iter().enumerate() {
        let child_hv = encode_tree(child, codebook, cache);
        // Position encoding via circular bit-rotation. XOR-bundling is
        // commutative, so a simple `xor_into(hv, role_vec ⊕ child)` would
        // lose child order. Rotating the child's HV by its position
        // breaks symmetry: rotate_left(A, 0) ⊕ rotate_left(B, 1) is not
        // equal to rotate_left(B, 0) ⊕ rotate_left(A, 1). Position is
        // recovered at unbind time via rotate_right.
        let permuted = rotate_left(&child_hv, i);
        xor_into(&mut hv, &permuted);
    }

    cache.put(key, hv);
    hv
}

/// Convenience: encode a tree against a fresh empty cache. For one-shot
/// queries; production code reuses a long-lived cache across calls.
pub fn encode_fresh<C>(node: &EncoderNode, codebook: &C) -> Hypervector
where
    C: BaseCodebook<Item = AstNodeFingerprint>,
{
    let cache = SubtreeCache::new();
    encode_tree(node, codebook, &cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codebook::AstCodebook;

    // Test convenience aliases — `leaf` and `node` are short stand-ins
    // for `EncoderNode::leaf` / `EncoderNode::new` to keep the test
    // bodies readable. Both forward directly with no extra logic.
    fn leaf(kind: CanonicalKind) -> EncoderNode {
        EncoderNode::leaf(kind)
    }

    fn node(kind: CanonicalKind, children: Vec<EncoderNode>) -> EncoderNode {
        EncoderNode::new(kind, children)
    }

    #[test]
    fn encoder_is_deterministic() {
        // Same tree → same hypervector. The content-hash cache must not
        // introduce any non-determinism (e.g. via HashMap iteration order
        // — which Rust's std HashMap does have, but our use is by-key
        // lookup, not iteration).
        let cb = AstCodebook::new();
        let tree = node(
            CanonicalKind::Block,
            vec![
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Stmt),
                node(CanonicalKind::Expr, vec![leaf(CanonicalKind::Lit)]),
            ],
        );
        let hv1 = encode_fresh(&tree, &cb);
        let hv2 = encode_fresh(&tree, &cb);
        assert_eq!(hv1, hv2);
    }

    #[test]
    fn rename_invariance_via_canonical_kind() {
        // Two trees that differ only in identifier names produce
        // structurally-identical hypervectors. The Deckard property —
        // identifiers/literals don't enter the encoding because they're
        // already collapsed at the canonical-kind level (both Ref).
        // Pin via two trees that have the same canonical structure.
        let cb = AstCodebook::new();
        let tree_a = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Ref), leaf(CanonicalKind::Lit)],
        );
        let tree_b = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Ref), leaf(CanonicalKind::Lit)],
        );
        assert_eq!(encode_fresh(&tree_a, &cb), encode_fresh(&tree_b, &cb));
    }

    #[test]
    fn child_order_changes_hypervector() {
        // Order *must* matter at the encoder level (even though the
        // codebook is order-invariant). The role-permutation step
        // (role_vector(i)) is what restores order-sensitivity. Two
        // trees with the same children in different positions must
        // produce different hypervectors — otherwise we'd lose the
        // ability to distinguish e.g. an `if` from a `do-while`.
        let cb = AstCodebook::new();
        let tree_a = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Block)],
        );
        let tree_b = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Block), leaf(CanonicalKind::Op)],
        );
        crate::util::assert_far_apart(
            &encode_fresh(&tree_a, &cb),
            &encode_fresh(&tree_b, &cb),
            "child order must affect HV",
        );
    }

    #[test]
    fn structurally_different_trees_far_apart() {
        // Two trees with different shapes (different node counts,
        // different kinds) should be far apart in Hamming space.
        // Random-pair baseline is ~D/2.
        let cb = AstCodebook::new();
        let tree_simple = leaf(CanonicalKind::Lit);
        let tree_complex = node(
            CanonicalKind::Block,
            vec![
                node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Ref)]),
                node(CanonicalKind::Expr, vec![leaf(CanonicalKind::Lit)]),
            ],
        );
        crate::util::assert_far_apart(
            &encode_fresh(&tree_simple, &cb),
            &encode_fresh(&tree_complex, &cb),
            "different shapes must be far apart",
        );
    }

    #[test]
    fn cache_dedups_identical_subtrees() {
        // Two functions that share an inner subtree should hit the cache.
        // Build a tree with a repeated subtree, encode, verify the cache
        // size reflects unique subtrees not total subtrees.
        let cb = AstCodebook::new();
        let inner = || {
            node(
                CanonicalKind::Stmt,
                vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Ref)],
            )
        };
        let tree = node(CanonicalKind::Block, vec![inner(), inner(), inner()]);
        let cache = SubtreeCache::new();
        encode_tree(&tree, &cb, &cache);
        // Unique subtrees: leaf Op, leaf Ref, inner Stmt, root Block = 4
        assert_eq!(
            cache.len(),
            4,
            "expected 4 unique subtrees in cache, got {}",
            cache.len(),
        );
    }

    #[test]
    fn content_hash_is_structure_only() {
        // content_hash must be a pure function of structure — two
        // identical trees produce the same hash, two different trees
        // produce different hashes (with overwhelming probability).
        let a = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Lit)],
        );
        let b = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Lit)],
        );
        let c = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Ref)],
        );
        assert_eq!(a.content_hash(), b.content_hash(), "identical structure must hash same");
        assert_ne!(a.content_hash(), c.content_hash(), "different structure must differ");
    }

    #[test]
    fn fresh_cache_starts_empty() {
        let cache = SubtreeCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn shared_cache_does_not_cross_pollute_across_codebooks() {
        // Skeptic-flagged bug (bead ley-line-open-4ba0cf): the
        // SubtreeCache key was content-only, so encoding the same
        // tree under two different codebooks would silently return
        // the first codebook's hypervector for both calls.
        //
        // Fix: cache_key() mixes codebook.codebook_tag() into the
        // hash. Same content + different tag → distinct keys.
        //
        // This test would have failed before the fix.
        use crate::codebook::{AstCodebook, ModuleCodebook};

        let cache = SubtreeCache::new();
        let tree = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Lit)],
        );

        let ast = AstCodebook::new();
        let module = ModuleCodebook::new();

        let hv_ast = encode_tree(&tree, &ast, &cache);
        let hv_module = encode_tree(&tree, &module, &cache);

        // Different codebooks → different hypervectors. Without the
        // fix, the second call would return hv_ast from the cache.
        assert_ne!(
            hv_ast, hv_module,
            "shared cache must not return one codebook's HV when queried by another",
        );

        // Cache must contain BOTH entries — one per codebook — not
        // a single entry shadowed by whichever ran first. The tree
        // has 3 nodes (Block + 2 leaves) so each codebook contributes
        // 3 cache entries.
        assert_eq!(
            cache.len(),
            6,
            "two codebooks × 3 nodes each = 6 distinct cache entries",
        );
    }

    #[test]
    fn role_binding_actually_bound() {
        // Sanity: encoding a 1-child tree must depend on the child's
        // hypervector, not just the parent's. If role-binding silently
        // dropped, two trees with the same parent but different
        // children would produce the same vector.
        let cb = AstCodebook::new();
        let parent_a = node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op)]);
        let parent_b = node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Lit)]);
        crate::util::assert_far_apart(
            &encode_fresh(&parent_a, &cb),
            &encode_fresh(&parent_b, &cb),
            "different child kinds must produce different parent HVs",
        );
    }

    #[test]
    fn unbind_recovers_child_from_position_zero() {
        // Bidi property: encode parent + 1 child at position 0, XOR
        // out the parent's base, and we get back the child's HV
        // directly (rotate_left by 0 is identity). This is the
        // cleanup-memory primitive op_hdc_unbind will use.
        let cb = AstCodebook::new();
        let child_kind = CanonicalKind::Op;
        let tree = node(CanonicalKind::Stmt, vec![leaf(child_kind)]);
        let parent_hv = encode_fresh(&tree, &cb);
        let child_hv_direct = encode_fresh(&leaf(child_kind), &cb);

        // For position 0: parent_hv = base ⊕ rotate_left(child, 0) = base ⊕ child
        // So child = parent ⊕ base. Use tree.fingerprint() so the
        // arity_bucket and sorted child kinds derive from the same
        // EncoderNode the encoder saw — the alternative (hand-built
        // AstNodeFingerprint) duplicated EncoderNode::fingerprint's
        // sort logic.
        let parent_base = cb.base_vector(&tree.fingerprint());

        let mut recovered = parent_hv;
        xor_into(&mut recovered, &parent_base);
        assert_eq!(
            recovered, child_hv_direct,
            "unbind must recover the child's exact hypervector for 1-child position-0",
        );
    }

    #[test]
    fn unbind_recovers_child_from_position_one() {
        // Same bidi property but for a child at position 1 — needs
        // rotate_right(•, 1) after stripping the position-0 child and
        // the parent's base.
        use crate::util::rotate_right;
        let cb = AstCodebook::new();
        let kind0 = CanonicalKind::Op;
        let kind1 = CanonicalKind::Lit;
        let tree = node(CanonicalKind::Stmt, vec![leaf(kind0), leaf(kind1)]);
        let parent_hv = encode_fresh(&tree, &cb);
        let child0_hv = encode_fresh(&leaf(kind0), &cb);
        let child1_hv = encode_fresh(&leaf(kind1), &cb);

        // Use tree.fingerprint() so the sort matches encode_tree's
        // input by construction — duplicating the sort_unstable_by_key
        // dance inline (as the prior version did) was a near-miss
        // for drift if the codebook ever changed its sort key.
        let parent_base = cb.base_vector(&tree.fingerprint());

        // parent = base ⊕ rotate_left(child0, 0) ⊕ rotate_left(child1, 1)
        //        = base ⊕ child0 ⊕ rotate_left(child1, 1)
        // Strip base + child0: leaves rotate_left(child1, 1).
        // rotate_right(•, 1) recovers child1.
        let mut residual = parent_hv;
        xor_into(&mut residual, &parent_base);
        xor_into(&mut residual, &child0_hv);
        let recovered = rotate_right(&residual, 1);
        assert_eq!(
            recovered, child1_hv,
            "rotate_right(residual, 1) must recover child at position 1",
        );
    }
}
