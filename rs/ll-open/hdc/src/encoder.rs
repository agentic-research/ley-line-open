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
use crate::util::{bucket_arity, rotate_left, xor_into, Hypervector};

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
        AstNodeFingerprint::new(
            self.canonical_kind,
            bucket_arity(self.children.len()),
            self.child_canonical_kinds_sorted.clone(),
        )
    }

    /// Content hash for cache lookup. Deterministic across machines —
    /// covers the kind, arity, sorted child kinds, AND each child's
    /// content hash, so structurally-identical subtrees collapse to
    /// the same hash regardless of where they appear in a tree.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.canonical_kind.discriminant()]);
        hasher.update(&[bucket_arity(self.children.len())]);
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
    use crate::codebook::{AstCodebook, ModuleCodebook};
    use crate::util::{assert_far_apart, rotate_right};

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
    fn encoder_node_leaf_constructor_pin() {
        // `EncoderNode::leaf(kind)` is the canonical "leaf" form —
        // zero children, kind preserved. Pin so a future refactor
        // that promoted leaf to "1 child of itself" or some clever
        // shortcut would shift produced base_vectors.
        let leaf_node = EncoderNode::leaf(CanonicalKind::Lit);
        assert_eq!(leaf_node.canonical_kind, CanonicalKind::Lit);
        assert!(leaf_node.children.is_empty());
        assert!(leaf_node.child_canonical_kinds_sorted.is_empty());

        // Should be byte-equivalent to `EncoderNode::new(kind, vec![])`.
        let manual = EncoderNode::new(CanonicalKind::Op, vec![]);
        let via_helper = EncoderNode::leaf(CanonicalKind::Op);
        assert_eq!(manual.canonical_kind, via_helper.canonical_kind);
        assert!(manual.children.is_empty());
        assert!(via_helper.children.is_empty());
        assert_eq!(
            manual.child_canonical_kinds_sorted,
            via_helper.child_canonical_kinds_sorted,
        );
    }

    #[test]
    fn encode_fresh_matches_encode_tree_on_fresh_cache() {
        // encode_fresh is documented as "encode a tree against a fresh
        // empty cache". Pin the equivalence so a refactor that, say,
        // pre-seeded the cache with a debug entry or wrapped the call
        // in something stateful would surface immediately.
        let cb = AstCodebook::new();
        let tree = node(
            CanonicalKind::Block,
            vec![
                leaf(CanonicalKind::Op),
                node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Lit)]),
            ],
        );
        let via_fresh = encode_fresh(&tree, &cb);
        let via_tree_with_fresh_cache = encode_tree(&tree, &cb, &SubtreeCache::new());
        assert_eq!(via_fresh, via_tree_with_fresh_cache);
    }

    #[test]
    fn encode_tree_leaf_equals_base_vector() {
        // For a leaf node (no children), encode_tree must produce
        // exactly the codebook's base_vector for that leaf — there
        // are no children to rotate-and-XOR. The for-loop in
        // encode_tree (line 159) iterates `node.children` zero times
        // for a leaf, so the accumulator equals the initial
        // `codebook.base_vector(&fp)` unchanged. Pin so a refactor
        // that always XOR'd a "phantom" role_vector(0) or initialized
        // to ZERO_HV instead of base_vector would surface immediately.
        let cb = AstCodebook::new();
        for kind in CanonicalKind::ALL {
            let leaf_node = leaf(kind);
            let encoded = encode_fresh(&leaf_node, &cb);
            let expected = cb.base_vector(&leaf_node.fingerprint());
            assert_eq!(
                encoded, expected,
                "encode_tree(leaf({kind:?})) must equal codebook.base_vector(fp)",
            );
        }
    }

    #[test]
    fn encoder_node_new_sorts_child_kinds_by_discriminant() {
        // EncoderNode::new sorts `child_canonical_kinds_sorted` (line
        // 36 `sorted.sort_unstable_by_key(|k| k.discriminant())`) so
        // the codebook receives a canonical, order-invariant child
        // sequence. Existing tests cover the downstream consequence
        // (child order doesn't affect base_vector) but never assert
        // the field is sorted directly. A refactor that dropped the
        // sort would still pass the codebook tests if the unsorted
        // order happened to coincide with the sorted one — pin the
        // invariant directly via a deliberately-unsorted child input.
        let children = vec![
            EncoderNode::leaf(CanonicalKind::Op),    // disc=6
            EncoderNode::leaf(CanonicalKind::Decl),  // disc=0
            EncoderNode::leaf(CanonicalKind::Stmt),  // disc=2
            EncoderNode::leaf(CanonicalKind::Block), // disc=3
        ];
        let node = EncoderNode::new(CanonicalKind::Stmt, children);
        // Sorted-by-discriminant: [Decl(0), Stmt(2), Block(3), Op(6)].
        let discs: Vec<u8> = node
            .child_canonical_kinds_sorted
            .iter()
            .map(|k| k.discriminant())
            .collect();
        assert_eq!(discs, vec![0, 2, 3, 6], "must be sorted by discriminant");
        // `children` (parser order) preserved separately:
        let children_discs: Vec<u8> = node
            .children
            .iter()
            .map(|c| c.canonical_kind.discriminant())
            .collect();
        assert_eq!(children_discs, vec![6, 0, 2, 3], "children must keep parser order");
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
        assert_far_apart(
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
        assert_far_apart(
            &encode_fresh(&tree_simple, &cb),
            &encode_fresh(&tree_complex, &cb),
            "different shapes must be far apart",
        );
    }

    #[test]
    fn encode_tree_cache_is_transparent() {
        // The cache is a performance optimization — warm-cache result
        // MUST equal cold-cache result for the same tree+codebook. A
        // refactor that mis-derived cache_key (e.g. dropped a field
        // from content_hash) could populate the cache with a value
        // distinct from what fresh encoding produces, silently making
        // cache hits return wrong HVs. Pin transparency directly.
        let cb = AstCodebook::new();
        let tree = node(
            CanonicalKind::Block,
            vec![
                node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op)]),
                leaf(CanonicalKind::Lit),
                node(CanonicalKind::Expr, vec![leaf(CanonicalKind::Ref)]),
            ],
        );
        let cold = encode_tree(&tree, &cb, &SubtreeCache::new());
        // Warm: pre-populate cache with the same tree, then encode
        // again. The second call should hit the cache for every node.
        let warm_cache = SubtreeCache::new();
        encode_tree(&tree, &cb, &warm_cache);
        let warm = encode_tree(&tree, &cb, &warm_cache);
        assert_eq!(cold, warm, "cache must be transparent to encode_tree output");
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
    fn content_hash_distinguishes_root_kind() {
        // The root canonical_kind discriminant is the first byte
        // hashed (encoder.rs:66). Two leaves with different kinds
        // must produce different content_hashes — this pins that
        // root-kind contribution. content_hash_is_structure_only
        // varies the children's kinds; this test varies the root
        // itself. A refactor that dropped the root-kind hashing
        // step would still pass on differently-shaped trees but
        // collapse all leaves of different kinds to one hash.
        let lits = leaf(CanonicalKind::Lit).content_hash();
        let decls = leaf(CanonicalKind::Decl).content_hash();
        let ops = leaf(CanonicalKind::Op).content_hash();
        assert_ne!(lits, decls, "different root kinds must hash differently");
        assert_ne!(lits, ops);
        assert_ne!(decls, ops);
    }

    #[test]
    fn content_hash_distinguishes_child_order() {
        // child_canonical_kinds_sorted is sorted (order-invariant at
        // the codebook lookup level), but content_hash also recurses
        // through `node.children` in PARSER order (encoder.rs:71-73),
        // so two trees with the same kinds in swapped order produce
        // different content_hashes. This is what makes the encoder's
        // rotation-based positional encoding consistent with
        // cache_key. A refactor that sorted node.children before
        // hashing would silently merge cache entries that should
        // stay separate.
        let a = node(
            CanonicalKind::Block,
            vec![
                node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op)]),
                leaf(CanonicalKind::Lit),
            ],
        );
        let b = node(
            CanonicalKind::Block,
            vec![
                leaf(CanonicalKind::Lit),
                node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op)]),
            ],
        );
        // Same kinds (sorted: Block, Stmt, Lit, Op...) but different
        // parser order → different content_hashes.
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn content_hash_distinguishes_nesting_from_flat() {
        // Same kinds, different topology: `Block[Op, Lit]` (flat,
        // 2 children) vs `Block[Op[Lit]]` (nested, 1 child whose
        // child is Lit). Total kind count is identical but the
        // shape differs. content_hash must distinguish these — the
        // recursive child-hash inclusion is what makes this work.
        // Catches a refactor that flattened the recursion or only
        // hashed kind-counts.
        let flat = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Lit)],
        );
        let nested = node(
            CanonicalKind::Block,
            vec![node(CanonicalKind::Op, vec![leaf(CanonicalKind::Lit)])],
        );
        assert_ne!(
            flat.content_hash(),
            nested.content_hash(),
            "flat vs nested topology must produce different content_hashes",
        );
    }

    #[test]
    fn content_hash_distinguishes_arity_within_same_bucket() {
        // bucket_arity collapses 3 and 4 children to the same bucket=3
        // (per util.rs:124). content_hash MUST still distinguish them
        // — the children-iteration step folds each child's content_
        // hash, so 3 vs 4 children produce different bytes. Pin so a
        // refactor that REPLACED `child_canonical_kinds_sorted` with
        // `bucket_arity(children.len())` in the hash would surface
        // immediately. content_hash is finer-grained than
        // `arity_bucket` by construction.
        let three = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Op), leaf(CanonicalKind::Op)],
        );
        let four = node(
            CanonicalKind::Block,
            vec![
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Op),
            ],
        );
        // Sanity: bucket_arity(3) == bucket_arity(4) (both bucket to 3).
        assert_eq!(
            crate::util::bucket_arity(3),
            crate::util::bucket_arity(4),
            "test premise: 3 and 4 children must bucket to same value",
        );
        // But content_hash must distinguish them.
        assert_ne!(three.content_hash(), four.content_hash());
    }

    #[test]
    fn content_hash_changes_with_arity_at_same_kind_set() {
        // Same canonical-kind composition, different arity at root.
        // Two trees that both produce {Block, Op, Op} but with
        // different topology — `Block[Op, Op]` (arity 2) vs the
        // `Block[Op[Op]]` shape — must produce different content
        // hashes because arity_bucket is part of the hash input.
        let arity_2 = node(
            CanonicalKind::Block,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Op)],
        );
        let arity_1_nested = node(
            CanonicalKind::Block,
            vec![node(CanonicalKind::Op, vec![leaf(CanonicalKind::Op)])],
        );
        assert_ne!(arity_2.content_hash(), arity_1_nested.content_hash());
    }

    #[test]
    fn cache_put_then_get_round_trips() {
        // The cache is a content-addressed map [u8;32] → Hypervector.
        // Pin the put/get contract: same key returns the value
        // unchanged. Catches a refactor that changed the storage
        // format (e.g. swapped key bytes order, or hashed the
        // bytes again on get).
        let cache = SubtreeCache::new();
        let key: [u8; 32] = [0xAB; 32];
        let value = crate::util::expand_seed(42);
        cache.put(key, value);
        assert_eq!(cache.get(&key), Some(value));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_get_unknown_key_returns_none() {
        // No phantom values for keys that were never put. Pin so a
        // refactor that defaulted to ZERO_HV or panicked on miss
        // would fail loudly.
        let cache = SubtreeCache::new();
        let unknown_key: [u8; 32] = [0u8; 32];
        assert_eq!(cache.get(&unknown_key), None);
    }

    #[test]
    fn fresh_cache_starts_empty() {
        let cache = SubtreeCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn subtree_cache_default_matches_new() {
        // `impl Default for SubtreeCache` delegates to `new()` (line
        // 125). Pin the equivalence so a refactor that pre-populated
        // the default cache (e.g. for a "warm start" optimization)
        // couldn't silently inject phantom entries that downstream
        // encoders would treat as legitimate cache hits.
        let via_default = SubtreeCache::default();
        assert!(via_default.is_empty());
        assert_eq!(via_default.len(), 0);
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
        assert_far_apart(
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
