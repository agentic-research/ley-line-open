//! Shared support for ADR-0030 Rung 2 (bead `ley-line-open-d50164`) —
//! the AST-structural value experiment. Included by the discrimination
//! test (`ast_structural_discrimination.rs`, Milestone A) and the
//! git-replay bench (`benches/git_replay_invalidation.rs`, Milestone B)
//! via `#[path]`, so both consult ONE definition of the stalk and the
//! oracle.
//!
//! Two ingredients:
//!
//! 1. **The rename-invariant AST-structural stalk.** Parse a region with
//!    tree-sitter (`leyline_ts`), take the PRE-ORDER SEQUENCE OF NAMED
//!    NODE KINDS (`node.kind()` — `"identifier"`, `"call_expression"`,
//!    `"if_expression"`, …; never the identifier TEXT), shingle it into
//!    kind-trigrams, map each trigram to a hypervector via the HDC
//!    substrate's `bytes_to_hv`, and majority-bundle. A rename touches
//!    zero node kinds → the stalk is invariant. A structural edit (new
//!    branch, an added statement, a changed node shape) perturbs the
//!    kind sequence → the stalk moves. This is the representation the
//!    byte-trigram stalk of Rung 1 could not be: Rung 1 measured that a
//!    rename moved the BYTE stalk FARTHER than a fact-changing edit,
//!    because byte distance tracks trigram churn, not structure.
//!
//!    KNOWN BLIND SPOT (measured, not hidden): a pure callee swap
//!    (`compute_weight` → `compute_penalty`) is INVISIBLE to any
//!    kind-structure embedding — both parse to `call_expression >
//!    identifier` — yet it DOES change `node_refs`. That is the
//!    structural false-negative Milestone B exists to quantify.
//!
//! 2. **The free oracle.** Re-derive the region's facts the way the live
//!    parse pipeline does: walk every named node and call
//!    `leyline_ts::refs::extract_rust`, collecting the emitted
//!    `node_defs` / `node_refs` / `_imports` tokens into a set. "Facts
//!    changed" == the before/after sets differ. This is the SAME
//!    emission the daemon persists (mirrors the `walk_and_insert` fold
//!    in `refs.rs`'s own fixtures), minus the position-dependent
//!    `node_id` / `container_node_id` path identity, which is not a
//!    derived FACT but a location.

#![allow(dead_code)] // each includer uses a subset

use leyline_hdc::util::bytes_to_hv;
use leyline_hdc::{D_BITS, D_BYTES, Hypervector, popcount_distance};
use leyline_ts::languages::TsLanguage;
use leyline_ts::refs::{ExtractedRef, extract_rust};
use std::collections::BTreeSet;
use tree_sitter::{Node, Parser, Tree};

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse Rust source into a tree-sitter tree. Returns `None` if the
/// grammar refuses the bytes (never observed on real `.rs` input, but
/// the git-replay corpus can hand us a half-written file mid-rebase).
pub fn parse_rust(src: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&TsLanguage::Rust.ts_language())
        .expect("tree-sitter-rust language must load");
    parser.parse(src, None)
}

// ---------------------------------------------------------------------------
// The rename-invariant AST-structural stalk
// ---------------------------------------------------------------------------

/// Pre-order sequence of NAMED node kinds. Identifier/literal TEXT never
/// enters — only `node.kind()` — so a rename is invisible here by
/// construction. This is the rename-invariance the experiment turns on.
pub fn kind_sequence(node: Node<'_>, out: &mut Vec<&'static str>) {
    // `Node::kind()` returns a grammar-interned `&'static str`, so the
    // sequence borrows nothing from the tree.
    out.push(node.kind());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        kind_sequence(child, out);
    }
}

/// The rename-invariant AST-structural stalk: a D-bit hypervector built
/// by kind-trigram shingle-bundle over the pre-order named-node kind
/// sequence.
///
/// Mirrors the HDC encoder's `leaf_content_hv` char-trigram construction
/// (Kanerva sequence encoding), but the alphabet is NODE KINDS, not
/// bytes. Two regions sharing most structural trigrams land close in
/// Hamming space; a rename shares ALL of them (distance 0); a new branch
/// introduces fresh trigrams (distance > 0).
pub fn structural_stalk_from_tree(tree: &Tree) -> Hypervector {
    let mut kinds: Vec<&'static str> = Vec::new();
    kind_sequence(tree.root_node(), &mut kinds);
    structural_stalk_from_kinds(&kinds)
}

/// Build the stalk from an explicit kind sequence (shared by tree + any
/// synthetic sequence).
pub fn structural_stalk_from_kinds(kinds: &[&str]) -> Hypervector {
    // Pad to at least one trigram (a 1- or 2-node region still has a
    // well-defined identity), mirroring `leaf_content_hv`'s short rule.
    let mut seq: Vec<&str> = kinds.to_vec();
    while seq.len() < 3 {
        seq.push("\u{0}pad");
    }

    let trigram_hvs: Vec<Hypervector> = seq
        .windows(3)
        .map(|w| {
            // Domain-separated tag; `\x1f` (unit separator) can't occur
            // inside a grammar kind name, so trigram boundaries are
            // unambiguous.
            let tag = format!("rung2-kindtri/{}\u{1f}{}\u{1f}{}", w[0], w[1], w[2]);
            bytes_to_hv(tag.as_bytes())
        })
        .collect();

    majority_bundle(&trigram_hvs)
}

/// Convenience: parse + stalk. Returns `None` on unparseable input.
pub fn structural_stalk(src: &[u8]) -> Option<Hypervector> {
    parse_rust(src).map(|t| structural_stalk_from_tree(&t))
}

/// Strict-majority bundle of N hypervectors: bit `i` set iff > half the
/// inputs have it set. Similarity-preserving. Identical to the Rung-1
/// bundle so the two rungs speak one representation.
pub fn majority_bundle(inputs: &[Hypervector]) -> Hypervector {
    let mut out = [0u8; D_BYTES];
    if inputs.is_empty() {
        return out;
    }
    let half = inputs.len() as u32 / 2;
    for bit in 0..D_BITS {
        let byte_idx = bit / 8;
        let bit_off = bit % 8;
        let mut count: u32 = 0;
        for hv in inputs {
            count += ((hv[byte_idx] >> bit_off) & 1) as u32;
        }
        if count > half {
            out[byte_idx] |= 1 << bit_off;
        }
    }
    out
}

/// Normalized Hamming distance `d/D` between two stalks. `0` = identical,
/// `≈ 0.5` = orthogonal (unrelated regions). Charikar: `d/D = θ/π`, so
/// this and cosine are interchangeable framings.
pub fn frac(a: &Hypervector, b: &Hypervector) -> f64 {
    popcount_distance(a, b) as f64 / D_BITS as f64
}

// ---------------------------------------------------------------------------
// The free oracle: re-derived facts (node_defs / node_refs / _imports)
// ---------------------------------------------------------------------------

/// The derived-fact SET for a region, re-computed via the live extractor.
///
/// Walk every named node and call `extract_rust` (the exact emission the
/// daemon fold persists), collecting canonicalized fact strings. Facts
/// are compared as a SET — we ask "did node_defs/node_refs/_imports
/// change," not "did their byte layout change." Position-only identity
/// (`node_id`, `container_node_id`) is deliberately excluded: it is a
/// location, not a fact, and shifts under any line insertion.
pub fn derive_facts(src: &[u8]) -> BTreeSet<String> {
    let mut facts = BTreeSet::new();
    let Some(tree) = parse_rust(src) else {
        return facts;
    };
    walk_facts(tree.root_node(), src, &mut facts);
    facts
}

/// Same fact set, but for a SUBTREE within an already-parsed file. The
/// git-replay bench uses this so `extract_rust` sees the region's real
/// impl/mod ancestors (the qualified `Recv::method` def arm reads the
/// enclosing `impl`), which a re-parse of the sliced-out function text
/// would lose. `src` must be the FULL file bytes the tree was parsed
/// from.
pub fn derive_facts_in_tree(node: Node<'_>, src: &[u8]) -> BTreeSet<String> {
    let mut facts = BTreeSet::new();
    walk_facts(node, src, &mut facts);
    facts
}

fn walk_facts(node: Node<'_>, src: &[u8], facts: &mut BTreeSet<String>) {
    // node_id/source_id are required by the signature but we discard the
    // position identity — only the emitted (kind, token) facts matter.
    let refs = extract_rust(&node, src, "n", "region.rs", None);
    for r in refs {
        match r {
            ExtractedRef::Def {
                token,
                canonical_kind,
                ..
            } => {
                facts.insert(format!(
                    "def\u{1f}{token}\u{1f}{}",
                    canonical_kind.unwrap_or("?")
                ));
            }
            ExtractedRef::Ref {
                token, qualifier, ..
            } => {
                facts.insert(format!(
                    "ref\u{1f}{token}\u{1f}{}",
                    qualifier.unwrap_or_default()
                ));
            }
            ExtractedRef::Import { alias, path, .. } => {
                facts.insert(format!("import\u{1f}{alias}\u{1f}{path}"));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_facts(child, src, facts);
    }
}

/// Did the region's derived facts change between two source versions?
/// This is the ground-truth oracle: `true` == a semantic optimizer that
/// skipped this edit would serve stale `node_defs`/`node_refs`.
pub fn facts_changed(before: &[u8], after: &[u8]) -> bool {
    derive_facts(before) != derive_facts(after)
}
