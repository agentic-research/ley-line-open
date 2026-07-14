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

use crate::D_BITS;
use crate::canonical::CanonicalKind;
use crate::codebook::{AstNodeFingerprint, BaseCodebook};
use crate::util::{Hypervector, ZERO_HV, bucket_arity, bytes_to_hv, rotate_left};

/// Char-trigram bundle for a token-bearing leaf. Bead `ley-line-open-98ac42`.
///
/// Builds a per-trigram HV via `bytes_to_hv` with a kind+trigram tag, then
/// majority-bundles them. Two tokens sharing N trigrams produce bundles
/// whose distance is roughly D/4 · (1 − N/total_trigrams).
///
/// Edge cases:
/// - **Empty content**: returns `bytes_to_hv` of the kind tag alone — a
///   stable identity for that kind. (Callers should avoid setting
///   `leaf_content: Some(empty)` — `leaf_with_content` filters this.)
/// - **Content shorter than 3 bytes**: pad with a sentinel byte 0x00 to
///   produce one trigram. A 1- or 2-char token has minimal but
///   well-defined identity.
/// - **Kind tag**: included so `Ref("foo")` and `Lit("foo")` produce
///   different HVs (same content, different role).
fn leaf_content_hv(kind: CanonicalKind, content: &[u8]) -> Hypervector {
    // Kind tag prefix is hashed into every trigram HV via the
    // `tagged_seed_vector`-style format `"hdc-leaf-{kind_disc}/{trigram_bytes}"`.
    // Without it, two leaves with the same text but different kinds
    // would produce identical hypervectors.
    let kind_byte = kind.discriminant();

    let mut buf = [0u8; 3];
    let mut trigram_hvs: Vec<Hypervector> = Vec::with_capacity(content.len() + 2);

    // Pad short content to at least 3 bytes via 0x00. Then we always have
    // at least one trigram regardless of content length.
    let padded = if content.len() < 3 {
        let mut p = content.to_vec();
        p.resize(3, 0u8);
        p
    } else {
        content.to_vec()
    };

    // Slide a 3-byte window across the padded content; each trigram HV is
    // seeded by [kind, trigram_bytes...]. The "hdc-leaf-trigram/" tag
    // prefix domain-separates from other `bytes_to_hv` callers.
    for window in padded.windows(3) {
        buf.copy_from_slice(window);
        let mut tag = Vec::with_capacity(b"hdc-leaf-trigram/".len() + 4);
        tag.extend_from_slice(b"hdc-leaf-trigram/");
        tag.push(kind_byte);
        tag.extend_from_slice(&buf);
        trigram_hvs.push(bytes_to_hv(&tag));
    }

    majority_bundle_with_tiebreak(&trigram_hvs)
}

/// Majority bundle of N hypervectors with a deterministic tiebreaker for
/// even-N — implements the HDC-canonical "similarity-preserving bundle."
///
/// Bead `ley-line-open-7b5086` (substrate rewrite). Replaces the prior
/// XOR-bind composition in `encode_tree` because XOR-bind is similarity-
/// destroying once per-level PRG draws (fingerprint-keyed `base_vector`,
/// content_hash-keyed `content_role`) are introduced — every interior node
/// adds a fresh random vector and XOR faithfully transmits the randomness
/// to the root. Bundle dampens that randomization (~1/F per level for
/// fan-out F) so structural edit distance shows up as Hamming distance.
///
/// Implementation detail: `HvCellComplex::bundle_majority` (sheaf.rs:379)
/// uses strict majority (`count > N/2`), which yields **intersection**
/// semantics on N=2 (loses similarity to both inputs). For the encoder we
/// need an HDC-style majority where a tie on a bit is broken by a per-call
/// deterministic vector — that gives output equally close to all inputs.
/// We pad with `tagged_seed_vector("hdc-bundle-tiebreak", 0)` when N is
/// even, then delegate to the strict-majority primitive.
fn majority_bundle_with_tiebreak(inputs: &[Hypervector]) -> Hypervector {
    if inputs.is_empty() {
        return ZERO_HV;
    }
    // Inline the strict-majority loop here (the version in sheaf.rs is on
    // `HvCellComplex` and pulling it in would add an unnecessary
    // dependency edge from encoder.rs to sheaf.rs).
    let strict_majority = |stalks: &[Hypervector]| -> Hypervector {
        let mut out = ZERO_HV;
        let half = stalks.len() as u32 / 2;
        for bit in 0..D_BITS {
            let byte_idx = bit / 8;
            let bit_off = bit % 8;
            let mut count: u32 = 0;
            for s in stalks {
                count += ((s[byte_idx] >> bit_off) & 1) as u32;
            }
            if count > half {
                out[byte_idx] |= 1 << bit_off;
            }
        }
        out
    };
    if inputs.len() % 2 == 1 {
        return strict_majority(inputs);
    }
    // Even N → pad with a deterministic tiebreaker so ties resolve to a
    // fixed pseudo-random direction rather than collapsing to 0.
    let tiebreak = bytes_to_hv(b"hdc-bundle-tiebreak/0");
    let mut padded = Vec::with_capacity(inputs.len() + 1);
    padded.extend_from_slice(inputs);
    padded.push(tiebreak);
    strict_majority(&padded)
}

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
    /// Children in their original (parser-given) order. Encoder
    /// rotates each child by position before bundle composition.
    pub children: Vec<EncoderNode>,
    /// Optional leaf token content — the raw byte text of an identifier,
    /// literal, or other token-bearing leaf. Bead `ley-line-open-98ac42`
    /// (seeded leaves): when present, the encoder produces a leaf HV via
    /// character-trigram bundle composition over the bytes instead of the
    /// kind-only `base_vector(fp)`. Two leaves sharing many trigrams get
    /// graded-close HVs (`Ref("getName")` and `Ref("getEmail")` share
    /// "get" → measurably closer than two random refs). Interior nodes
    /// keep this as `None` — only token-bearing leaves use it.
    pub leaf_content: Option<Vec<u8>>,
}

impl EncoderNode {
    /// Convenience constructor for interior nodes (with children). Leaves
    /// the `leaf_content` field as `None` — interior nodes never carry
    /// token text.
    pub fn new(kind: CanonicalKind, children: Vec<EncoderNode>) -> Self {
        let mut sorted: Vec<CanonicalKind> = children.iter().map(|c| c.canonical_kind).collect();
        sorted.sort_unstable_by_key(|k| k.discriminant());
        EncoderNode {
            canonical_kind: kind,
            child_canonical_kinds_sorted: sorted,
            children,
            leaf_content: None,
        }
    }

    /// Convenience constructor for a kind-only leaf node (no children,
    /// no token content). Encoder uses `base_vector(fp)` at this leaf.
    pub fn leaf(kind: CanonicalKind) -> Self {
        Self::new(kind, vec![])
    }

    /// Convenience constructor for a content-bearing leaf node.
    /// Used for `Ref`/`Lit`-style leaves where the token bytes carry
    /// load-bearing semantic content. Encoder produces a graded-similar
    /// HV via character-trigram bundle composition over `content`.
    ///
    /// Empty `content` is allowed (degenerates to `leaf(kind)`'s
    /// behavior — content-less leaf).
    pub fn leaf_with_content(kind: CanonicalKind, content: Vec<u8>) -> Self {
        let mut node = Self::leaf(kind);
        if !content.is_empty() {
            node.leaf_content = Some(content);
        }
        node
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
        // Length-prefixed blocks match the discipline in
        // `codebook::canonical_signature_bytes` (codebook.rs:159).
        // Without the prefixes, a child blake3 starting with valid
        // CanonicalKind discriminant bytes (values 0..6, p=7/256 per
        // byte) could in principle alias the boundary between the
        // kinds block and the children-hashes block — making two
        // distinct EncoderNode trees collide.
        //
        // Probability is ~2^-50 for natural code and irrelevant
        // cryptographically; the fix exists to keep `content_hash` a
        // strict structural witness so the SubtreeCache contract
        // ("same tree → same key") holds without "natural-code"
        // qualifiers. Fix surfaced by math-friend review on bead
        // `ley-line-open-641809` (Q4).
        //
        // Safe to land at any time: this is a cache-key hash, not a
        // codebook output. Changing it invalidates in-process
        // `SubtreeCache` entries (they self-heal on the next encode),
        // but does NOT invalidate persisted `_hdc` rows — those store
        // codebook outputs computed deterministically from the same
        // EncoderNode regardless of which key was used to cache them.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.canonical_kind.discriminant()]);
        hasher.update(&[bucket_arity(self.children.len())]);

        let kinds_len = self.child_canonical_kinds_sorted.len() as u16;
        hasher.update(&kinds_len.to_le_bytes());
        for k in &self.child_canonical_kinds_sorted {
            hasher.update(&[k.discriminant()]);
        }

        let children_len = self.children.len() as u16;
        hasher.update(&children_len.to_le_bytes());
        for child in &self.children {
            hasher.update(&child.content_hash());
        }

        // Bead `ley-line-open-98ac42`: leaf_content discriminates two
        // leaves of the same kind with different token bytes. Without
        // this hashed in, the cache would return the same HV for
        // `Ref("getName")` and `Ref("getEmail")` despite the encoder
        // producing different HVs at the leaf step. Length-prefixed
        // for the same anti-aliasing reason as `child_canonical_kinds`.
        match &self.leaf_content {
            Some(c) => {
                let len = c.len() as u32;
                hasher.update(&len.to_le_bytes());
                hasher.update(c);
            }
            None => {
                hasher.update(&0u32.to_le_bytes());
            }
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
        self.map.lock().expect("mutex poisoned").get(key).copied()
    }

    pub fn put(&self, key: [u8; 32], hv: Hypervector) {
        self.map.lock().expect("mutex poisoned").insert(key, hv);
    }

    pub fn len(&self) -> usize {
        self.map.lock().expect("mutex poisoned").len()
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
    let base = codebook.base_vector(&fp);

    // Seeded leaves (bead ley-line-open-98ac42). A content-bearing leaf
    // (Ref/Lit token with text) gets encoded via char-trigram bundle so
    // two leaves sharing many substrings produce graded-close HVs.
    //
    // Why this matters: without seeded leaves, `Ref("getName")` and
    // `Ref("getEmail")` are bitwise-identical (kind=Ref, arity=0, no
    // children → same fingerprint → same base_vector). The bundle
    // composition can't help if the leaves it composes are atomic
    // hashes with no metric. Third-party HDC review made this point
    // directly: "if your leaf tokens are random/orthogonal, you only
    // get credit for exactly shared tokens."
    //
    // Char-trigram bundle is the classic HDC text-encoding pattern
    // (Kanerva 2009 sequence encoding). For each 3-char window of the
    // token bytes, derive an HV via `tagged_seed_vector("hdc-trigram", ...)`
    // and majority-bundle them all. Two tokens sharing N trigrams produce
    // bundles whose Hamming distance is roughly inversely proportional
    // to N — graded similarity in the substrate's natural metric.
    //
    // Leaves with no content (`leaf_content: None`) keep the existing
    // `base_vector(fp)` behavior — unchanged for non-token-bearing
    // leaves like Block-shaped scopes.
    if node.children.is_empty()
        && let Some(content) = node.leaf_content.as_ref()
        && !content.is_empty()
    {
        let hv = leaf_content_hv(node.canonical_kind, content);
        cache.put(key, hv);
        return hv;
    }

    // Bundle composition (bead ley-line-open-7b5086, math-friend session-3 +
    // third-party HDC review). Replaces the prior XOR-bind composition.
    //
    // Old composition: `hv = base ⊕ ⊕_i rotate_left(child ⊕ content_role, i)`.
    // XOR-bind faithfully transmits any per-level randomness (the
    // fingerprint-keyed base_vector switch + the content_role term added in
    // PR #94) to the root, so one-leaf-different trees become orthogonal.
    //
    // New composition: `hv = majority_bundle({base} ∪ {rotate_left(child_i, i)})`.
    // Bundle is similarity-DAMPENING: bundle({A, B}) sits at ≈D/4 from both
    // A and B (vs D/2 for XOR). Per math-friend session-3, a one-leaf
    // perturbation that's D/2 at the leaf becomes ≈ D/(F^depth) at the root
    // for fan-out F — measurable signal at typical AST shapes (depth 5-7,
    // fan-out 2-5).
    //
    // Position is still encoded via `rotate_left(child_hv, i)` — bundle is
    // commutative so order must come from the inputs being permuted.
    //
    // `content_role` is intentionally DROPPED. Math-friend session-3: under
    // bundle composition it's just a second fresh-PRG draw per node, hurting
    // the same way base_vector switching did. The Merkle-reversibility claim
    // (PR #94 substrate property) doesn't survive bundle composition since
    // bundle isn't invertible; that property is retired together with the
    // unbind algebra and its tests.
    let mut bundle_inputs: Vec<Hypervector> = Vec::with_capacity(node.children.len() + 1);
    bundle_inputs.push(base);
    for (i, child) in node.children.iter().enumerate() {
        let child_hv = encode_tree(child, codebook, cache);
        bundle_inputs.push(rotate_left(&child_hv, i));
    }
    let hv = majority_bundle_with_tiebreak(&bundle_inputs);

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
    use crate::util::assert_far_apart;

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
        assert_eq!(
            children_discs,
            vec![6, 0, 2, 3],
            "children must keep parser order"
        );
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
        // Order *must* matter at the encoder level. The positional
        // `rotate_left(child, i)` inputs to majority-bundle are what
        // restore order-sensitivity (bundle is otherwise commutative).
        //
        // Threshold update for bead `ley-line-open-7b5086`: under bundle
        // composition the encoded HVs are similarity-dampened — order-
        // flipped trees are at ~D/2 - 1000 instead of D/2 (random
        // baseline). The `assert_far_apart` threshold of 3500 was
        // calibrated for XOR-bind where any structural change went to
        // D/2. The PROPERTY (order matters) still holds; the magnitude
        // shrank by design (bundle dampens upstream randomization).
        //
        // We use a relative gate: distance(A, B) must exceed
        // `D_BITS / 8` = 1024 bits (the noise floor at bundle's
        // dampening regime for fan-out 2 at depth 2). On real encoders
        // this is ~2000-3000 bits.
        let cb = AstCodebook::new();
        let tree_a = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Block)],
        );
        let tree_b = node(
            CanonicalKind::Stmt,
            vec![leaf(CanonicalKind::Block), leaf(CanonicalKind::Op)],
        );
        let hv_a = encode_fresh(&tree_a, &cb);
        let hv_b = encode_fresh(&tree_b, &cb);
        let d = crate::util::popcount_distance(&hv_a, &hv_b);
        assert!(
            d > 1024,
            "child order must affect HV measurably under bundle composition; \
             got distance {d} (threshold 1024 bits; XOR baseline was 3500)"
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
                node(
                    CanonicalKind::Stmt,
                    vec![leaf(CanonicalKind::Op), leaf(CanonicalKind::Ref)],
                ),
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
        assert_eq!(
            cold, warm,
            "cache must be transparent to encode_tree output"
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
        assert_eq!(
            a.content_hash(),
            b.content_hash(),
            "identical structure must hash same"
        );
        assert_ne!(
            a.content_hash(),
            c.content_hash(),
            "different structure must differ"
        );
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
            vec![
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Op),
                leaf(CanonicalKind::Op),
            ],
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
    fn cache_put_overwrites_on_duplicate_key() {
        // SubtreeCache::put delegates to HashMap::insert, which
        // replaces. Pin the overwrite contract: same key inserted
        // twice → len stays 1, get returns the second value. A
        // refactor that switched to entry().or_insert (which would
        // keep the FIRST value) would silently mean a re-encoded
        // subtree never updates the cache, even if its content
        // changed (impossible in production thanks to content-hash
        // keys, but a real bug if the cache were ever used as a
        // mutable store).
        let cache = SubtreeCache::new();
        let key: [u8; 32] = [0xCD; 32];
        let v1 = crate::util::expand_seed(1);
        let v2 = crate::util::expand_seed(2);
        cache.put(key, v1);
        cache.put(key, v2);
        assert_eq!(cache.len(), 1, "duplicate key must not grow cache");
        assert_eq!(cache.get(&key), Some(v2), "second put must win");
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
        // hypervector, not just the parent's. If the composition silently
        // dropped the child, two trees with the same parent but different
        // children would produce the same vector.
        //
        // Bead `ley-line-open-7b5086`: under bundle composition, two
        // different child kinds bring different inputs into the bundle,
        // so the result HV differs measurably from the parent-only HV.
        // Magnitude is smaller than under XOR-bind (bundle dampens), so
        // the threshold is relaxed from 3500 to 1024 bits.
        let cb = AstCodebook::new();
        let parent_a = node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Op)]);
        let parent_b = node(CanonicalKind::Stmt, vec![leaf(CanonicalKind::Lit)]);
        let d = crate::util::popcount_distance(
            &encode_fresh(&parent_a, &cb),
            &encode_fresh(&parent_b, &cb),
        );
        assert!(
            d > 1024,
            "different child kinds must produce measurably different parent HVs; \
             got distance {d} (threshold 1024 bits under bundle composition)"
        );
    }

    #[test]
    fn seeded_leaves_grade_similar_tokens() {
        // Bead `ley-line-open-98ac42`: token-bearing leaves carry text via
        // `leaf_content`. The encoder produces char-trigram bundle HVs at
        // those leaves so two leaves sharing substrings get measurably-
        // close HVs while unrelated tokens stay far.
        let cb = AstCodebook::new();

        // Same kind, shared prefix "get" → trigrams "get","etN","tNa",...
        // overlap with "get","etE","tEm",... at the leading trigram.
        let get_name = EncoderNode::leaf_with_content(CanonicalKind::Ref, b"getName".to_vec());
        let get_email = EncoderNode::leaf_with_content(CanonicalKind::Ref, b"getEmail".to_vec());
        // Same kind, no shared substrings — should be far.
        let unrelated = EncoderNode::leaf_with_content(CanonicalKind::Ref, b"xyzqqq".to_vec());
        // Same text, different kind — should be far (kind tag domain-
        // separates the trigram seeds).
        let lit_get_name = EncoderNode::leaf_with_content(CanonicalKind::Lit, b"getName".to_vec());

        let h_get_name = encode_fresh(&get_name, &cb);
        let h_get_email = encode_fresh(&get_email, &cb);
        let h_unrelated = encode_fresh(&unrelated, &cb);
        let h_lit_get_name = encode_fresh(&lit_get_name, &cb);

        let d_shared = crate::util::popcount_distance(&h_get_name, &h_get_email);
        let d_unrelated = crate::util::popcount_distance(&h_get_name, &h_unrelated);
        let d_diff_kind = crate::util::popcount_distance(&h_get_name, &h_lit_get_name);

        println!(
            "d(Ref'getName', Ref'getEmail') = {d_shared}, \
             d(Ref'getName', Ref'xyzqqq') = {d_unrelated}, \
             d(Ref'getName', Lit'getName') = {d_diff_kind}"
        );

        // Shared-prefix MUST be closer than unrelated.
        assert!(
            d_shared < d_unrelated,
            "shared-prefix leaves must be closer than unrelated leaves; \
             got d_shared={d_shared}, d_unrelated={d_unrelated}"
        );
        // Different-kind same-content MUST be far (kind tag prevents collision).
        assert!(
            d_diff_kind > 1024,
            "leaves of different canonical kinds must be far even with same content; \
             got d_diff_kind={d_diff_kind}"
        );
    }

    #[test]
    fn seeded_leaf_with_same_content_is_deterministic() {
        // Bead `ley-line-open-98ac42`: same content + same kind must
        // produce bit-identical HV across two encodings. Pinned so the
        // SubtreeCache contract holds (cache returns the same HV that
        // a fresh encode would).
        let cb = AstCodebook::new();
        let a = EncoderNode::leaf_with_content(CanonicalKind::Ref, b"foo".to_vec());
        let b = EncoderNode::leaf_with_content(CanonicalKind::Ref, b"foo".to_vec());
        assert_eq!(encode_fresh(&a, &cb), encode_fresh(&b, &cb));
    }

    #[test]
    fn seeded_leaf_with_empty_content_falls_back_to_base_vector() {
        // `leaf_with_content(kind, vec![])` filters empty content,
        // leaving `leaf_content: None`. The encoder then uses
        // `base_vector(fp)` — equivalent to the kind-only `leaf(kind)`
        // path. Pin that backward-compat behavior.
        let cb = AstCodebook::new();
        let empty = EncoderNode::leaf_with_content(CanonicalKind::Ref, vec![]);
        let kind_only = EncoderNode::leaf(CanonicalKind::Ref);
        assert_eq!(encode_fresh(&empty, &cb), encode_fresh(&kind_only, &cb));
    }

    // Unbind tests removed: bead `ley-line-open-7b5086` swapped XOR-bind
    // composition for majority-bundle composition. Majority bundle is not
    // invertible (no `unbind` operation exists for it without an explicit
    // cleanup-memory codebook of all candidate children), so the prior
    // tests — `unbind_recovers_child_from_position_zero/one`,
    // `unbind_rejects_wrong_content_hash`, and the related
    // `child_order_still_changes_hv_after_merkle_rebind` — assert algebra
    // the new encoder no longer supports. Their substrate-property claim
    // (Merkle-reversibility from PR #94) is retired together with
    // `content_role`. Order-sensitivity is still tested by
    // `child_order_changes_hypervector` above.
}
