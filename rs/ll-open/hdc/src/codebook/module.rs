//! Module-layer codebook: encodes file/module-scope *composition* —
//! the set of top-level declarations a file contains, not the bodies
//! of those declarations.
//!
//! Distinct from the AST codebook in scope: AstCodebook hashes the
//! recursive structure of one function (control flow, expressions,
//! statements). ModuleCodebook hashes the *header* of a file: which
//! kinds of declarations exist at the top level, in what positional
//! arrangement.
//!
//! Two files with `[fn_A, type_B, fn_C]` and `[fn_X, type_Y, fn_Z]`
//! produce close hypervectors (same composition: 2 funcs + 1 type, in
//! that order). A file with `[fn_A]` vs `[type_B, type_C, type_D]`
//! produces far hypervectors.
//!
//! Why this layer matters: AstCodebook detects function-level clones.
//! ModuleCodebook detects file-level *layouts* — boilerplate files,
//! mirror directories (`mod.rs` + `mod_test.rs`), repeated scaffolding
//! across crates.

use crate::codebook::{canonical_signature_bytes, AstNodeFingerprint, BaseCodebook};
use crate::encoder::EncoderNode;
use crate::util::{
    bucket_arity, bytes_to_hv, popcount_distance, rotate_left, xor_into,
    Hypervector, ZERO_HV,
};

#[cfg(test)]
use crate::canonical::CanonicalKind;

/// Module-layer codebook. Stateless; reproducible across machines.
pub struct ModuleCodebook;

impl Default for ModuleCodebook {
    fn default() -> Self {
        ModuleCodebook
    }
}

impl ModuleCodebook {
    pub fn new() -> Self {
        Self
    }

    /// Build a "header signature" for a single top-level decl: its
    /// canonical kind + arity bucket + the canonical kinds of its
    /// immediate named children. This captures the decl's *shape*
    /// without recursing into its body — a function and its body are
    /// the AST layer's job, not the module layer's.
    ///
    /// Delegates to the shared `codebook::canonical_signature_bytes` so
    /// the byte layout matches the AST layer; only the inputs differ
    /// (decl-header data here, full-fingerprint data there).
    fn header_signature_bytes(node: &EncoderNode) -> Vec<u8> {
        let child_kinds: Vec<_> = node.children.iter().map(|c| c.canonical_kind).collect();
        canonical_signature_bytes(
            "hdc-module",
            node.canonical_kind,
            bucket_arity(node.children.len()),
            &child_kinds,
        )
    }
}

/// `BaseCodebook` impl exists so the trait surface stays uniform across
/// layers (any consumer that takes `Box<dyn BaseCodebook>` works) — but
/// the AST encoder doesn't recurse into module-level fingerprints. The
/// canonical entry point is `encode_module` below, which only encodes
/// the file-root's immediate children.
impl BaseCodebook for ModuleCodebook {
    type Item = AstNodeFingerprint;

    fn codebook_tag(&self) -> &'static str {
        "hdc-module"
    }

    fn base_vector(&self, item: &Self::Item) -> Hypervector {
        // Shares the canonical signature byte layout with AstCodebook
        // but uses the "hdc-module" tag so the resulting hypervector
        // is distinct from AstCodebook's even on the same fingerprint.
        // (Skeptic-review bead 4bb8a0: identical base_vector across
        // codebooks would mean ModuleCodebook silently produces
        // AstCodebook output via encode_tree.)
        let buf = canonical_signature_bytes(
            "hdc-module",
            item.canonical_kind,
            item.arity_bucket,
            &item.child_canonical_kinds,
        );
        bytes_to_hv(&buf)
    }

    // role_vector: uses the trait default (codebook_tag + "-role").
    // Default produces tag "hdc-module-role" — byte-identical to the
    // previous explicit override (skeptic 4bbc54 dedup).
}

/// Encode a tree at the *module* level: only the file-root's immediate
/// children contribute. Each top-level decl's header (kind + arity +
/// immediate child kinds) becomes one base vector, rotated by its
/// position, XOR-bundled into an accumulator that starts with the
/// file-root's own kind vector.
///
/// Crucially: does NOT recurse into decl bodies. A function's body
/// (control flow, expressions, statements) belongs to the AST layer.
/// The module layer captures *what the file contains*, not *how those
/// things work internally*.
pub fn encode_module(tree: &EncoderNode, _cb: &ModuleCodebook) -> Hypervector {
    // Start with the root container's kind as the file-level identity.
    let mut hv = bytes_to_hv(&[
        tree.canonical_kind.discriminant(),
        b'M', // domain tag so module-root vectors don't collide with AST-leaf vectors
    ]);

    for (i, child) in tree.children.iter().enumerate() {
        let sig = ModuleCodebook::header_signature_bytes(child);
        let child_hv = bytes_to_hv(&sig);
        let permuted = rotate_left(&child_hv, i);
        xor_into(&mut hv, &permuted);
    }

    hv
}

/// Module-level Hamming distance between two trees. Convenience wrapper
/// for `popcount_distance(encode_module(a), encode_module(b))`.
pub fn module_distance(a: &EncoderNode, b: &EncoderNode, cb: &ModuleCodebook) -> u32 {
    let ha = encode_module(a, cb);
    let hb = encode_module(b, cb);
    popcount_distance(&ha, &hb)
}

/// Identity helper — useful for callers building a custom XOR fold.
pub fn module_zero() -> Hypervector {
    ZERO_HV
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codebook::AstCodebook;
    use crate::util::{assert_far_apart, tagged_seed_vector};

    // Test convenience aliases mirroring encoder.rs — both forward
    // directly to `EncoderNode::leaf` / `EncoderNode::new`.
    fn leaf(kind: CanonicalKind) -> EncoderNode {
        EncoderNode::leaf(kind)
    }

    fn node(kind: CanonicalKind, children: Vec<EncoderNode>) -> EncoderNode {
        EncoderNode::new(kind, children)
    }

    fn fake_func() -> EncoderNode {
        // A "function-shaped" top-level decl: Decl with a Block body
        // and a Ref param. The bodies (Stmts/Exprs/Lits inside) are
        // ignored at module level — that's AST's territory.
        node(
            CanonicalKind::Decl,
            vec![leaf(CanonicalKind::Ref), leaf(CanonicalKind::Block)],
        )
    }

    fn fake_type() -> EncoderNode {
        // A "type-shaped" top-level decl: Decl with a Ref name and a
        // Block of fields.
        node(
            CanonicalKind::Decl,
            vec![leaf(CanonicalKind::Ref), leaf(CanonicalKind::Block)],
        )
    }

    fn fake_var() -> EncoderNode {
        // A "var-shaped" decl: Decl with just a Ref + Lit (no block).
        node(
            CanonicalKind::Decl,
            vec![leaf(CanonicalKind::Ref), leaf(CanonicalKind::Lit)],
        )
    }

    #[test]
    fn module_encoder_is_deterministic() {
        let cb = ModuleCodebook::new();
        let tree = node(
            CanonicalKind::Block,
            vec![fake_func(), fake_type(), fake_var()],
        );
        let h1 = encode_module(&tree, &cb);
        let h2 = encode_module(&tree, &cb);
        assert_eq!(h1, h2);
    }

    #[test]
    fn module_encoder_does_not_recurse_into_bodies() {
        // Two files whose top-level decls have IDENTICAL headers but
        // DIFFERENT bodies must produce identical module hypervectors.
        // This is the property that distinguishes module from AST: at
        // module level, the body shape is irrelevant.
        let cb = ModuleCodebook::new();

        // File A: one function with body [Stmt, Stmt]
        let file_a = node(
            CanonicalKind::Block,
            vec![node(
                CanonicalKind::Decl,
                vec![
                    leaf(CanonicalKind::Ref),
                    node(
                        CanonicalKind::Block,
                        vec![leaf(CanonicalKind::Stmt), leaf(CanonicalKind::Stmt)],
                    ),
                ],
            )],
        );

        // File B: one function with body [Stmt, Stmt, Stmt, Op] — different body
        let file_b = node(
            CanonicalKind::Block,
            vec![node(
                CanonicalKind::Decl,
                vec![
                    leaf(CanonicalKind::Ref),
                    node(
                        CanonicalKind::Block,
                        vec![
                            leaf(CanonicalKind::Stmt),
                            leaf(CanonicalKind::Stmt),
                            leaf(CanonicalKind::Stmt),
                            leaf(CanonicalKind::Op),
                        ],
                    ),
                ],
            )],
        );

        // Both top-level decls have header (Decl, arity 2, children=[Ref, Block]).
        // Module-level HV should be identical despite different bodies.
        let ha = encode_module(&file_a, &cb);
        let hb = encode_module(&file_b, &cb);
        assert_eq!(ha, hb, "module HV must ignore decl bodies");
    }

    #[test]
    fn module_decl_order_changes_hv() {
        // Same set of decls in different order produces different HVs —
        // the rotation positional encoding preserves order.
        let cb = ModuleCodebook::new();
        let order_1 = node(
            CanonicalKind::Block,
            vec![fake_func(), fake_type(), fake_var()],
        );
        let order_2 = node(
            CanonicalKind::Block,
            vec![fake_var(), fake_func(), fake_type()],
        );
        assert_far_apart(
            &encode_module(&order_1, &cb),
            &encode_module(&order_2, &cb),
            "module: declaration order must affect HV",
        );
    }

    #[test]
    fn module_decl_count_changes_hv() {
        // Adding a decl to the file changes the HV (unless the new decl
        // is at a position whose rotation cancels — extremely unlikely
        // by design). Membership is part of module identity.
        let cb = ModuleCodebook::new();
        let file_1 = node(CanonicalKind::Block, vec![fake_func(), fake_func()]);
        let file_2 = node(
            CanonicalKind::Block,
            vec![fake_func(), fake_func(), fake_func()],
        );
        assert_far_apart(
            &encode_module(&file_1, &cb),
            &encode_module(&file_2, &cb),
            "module: adding a decl must change HV",
        );
    }

    #[test]
    fn module_root_kind_changes_hv() {
        // Two files with the same top-level decls but different file-root
        // kinds (e.g. Block vs Stmt) produce different HVs. This catches
        // a refactor that drops the root from the encoding.
        let cb = ModuleCodebook::new();
        let as_block = node(CanonicalKind::Block, vec![fake_func()]);
        let as_stmt = node(CanonicalKind::Stmt, vec![fake_func()]);
        assert_far_apart(
            &encode_module(&as_block, &cb),
            &encode_module(&as_stmt, &cb),
            "module: root kind must contribute to HV",
        );
    }

    #[test]
    fn module_role_vector_distinct_from_ast_role() {
        // Domain tags ("hdc-module-role" vs "hdc-ast-role") must produce
        // distinct hypervectors per role index. If they collide, an
        // unbind in a multi-layer combined view could mis-route a child
        // hypervector to the wrong layer.
        let module_cb = ModuleCodebook::new();
        let ast_cb = AstCodebook::new();
        let m_role0 = module_cb.role_vector(0);
        let a_role0 = ast_cb.role_vector(0);
        assert_far_apart(&m_role0, &a_role0, "module role-0 must not collide with AST role-0");
    }

    #[test]
    fn module_role_vector_default_matches_explicit_tag() {
        // Skeptic 4bbc54: deleted the explicit override on
        // ModuleCodebook, relying on the trait default to derive
        // "hdc-module-role" from codebook_tag(). Pin that the default
        // produces byte-identical output to the previous explicit
        // `tagged_seed_vector("hdc-module-role", N)` form.
        let cb = ModuleCodebook::new();
        for i in [0usize, 1, 7, 42, 1024] {
            let actual = cb.role_vector(i);
            let expected = tagged_seed_vector("hdc-module-role", i);
            assert_eq!(
                actual, expected,
                "role_vector({i}) must match tagged_seed_vector(\"hdc-module-role\", {i})"
            );
        }
    }

    #[test]
    fn module_distance_zero_for_identical_files() {
        let cb = ModuleCodebook::new();
        let file = node(
            CanonicalKind::Block,
            vec![fake_func(), fake_type(), fake_var()],
        );
        let copy = node(
            CanonicalKind::Block,
            vec![fake_func(), fake_type(), fake_var()],
        );
        assert_eq!(module_distance(&file, &copy, &cb), 0);
    }

    #[test]
    fn module_zero_is_zero_hv() {
        // Sanity: the identity helper returns the canonical zero
        // hypervector. Drift guard if a future refactor accidentally
        // changes the constant.
        assert_eq!(module_zero(), ZERO_HV);
    }
}
