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
use leyline_sheaf::restriction_cache::{
    ApiDefRow, ContainerKeying, DefRow, FactSubstrate, ImportRow, RefRow, US,
};
use leyline_ts::languages::TsLanguage;
use leyline_ts::refs::{ExtractedRef, extract_rust};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

// ===========================================================================
// Restriction-addressed review cache (ADR-0031, bead ley-line-open-054048)
//
// Parser-dependent extraction that populates a parser-INDEPENDENT
// `leyline_sheaf::restriction_cache::FactSubstrate` from Rust source. This
// is the tree-sitter stand-in for the daemon's node_defs / node_refs /
// _imports fact tables. It lives here (dev/bench shared) so the real-facts
// test, the git-replay bench (`f3a81e`), and the second review family
// (`f463aa`) all consult ONE extraction, and `leyline-sheaf` itself stays
// parser-independent in production.
// ===========================================================================

/// Build a [`FactSubstrate`] from a multi-file corpus, deriving each
/// function's container identity per `keying`. Fact rows are de-duplicated
/// as sets (multiplicity of identical call sites does not enter the
/// restriction or the review — the same set semantics as the rung-2
/// oracle).
pub fn build_substrate(corpus: &[(String, String)], keying: ContainerKeying) -> FactSubstrate {
    let mut defs = BTreeSet::new();
    let mut refs = BTreeSet::new();
    let mut imports = BTreeSet::new();
    let mut container_by_fn: BTreeMap<String, String> = BTreeMap::new();
    for (path, src) in corpus {
        let tree = parse_rust(src.as_bytes()).expect("fixture source must parse");
        walk_extract(
            tree.root_node(),
            src.as_bytes(),
            path,
            path, // root node_id == source_id, mirroring cmd_parse's fold
            None,
            keying,
            &mut defs,
            &mut refs,
            &mut imports,
            &mut container_by_fn,
        );
    }
    FactSubstrate::from_rows(
        defs.into_iter().collect(),
        refs.into_iter().collect(),
        imports.into_iter().collect(),
        container_by_fn,
    )
}

/// Per-named-node fold mirroring the daemon's content-addressing walk.
/// `extract_rust` is anchored (only patterns rooted at the node emit); the
/// nearest enclosing `function_item`'s identity is threaded to its
/// descendants as the container. `node_id` is the daemon-style positional
/// AST path of `node` (`{parent}/{kind}[_{idx}]`), needed for
/// [`ContainerKeying::Positional`].
#[allow(clippy::too_many_arguments)]
fn walk_extract(
    node: Node<'_>,
    src: &[u8],
    source_id: &str,
    node_id: &str,
    container: Option<&str>,
    keying: ContainerKeying,
    defs: &mut BTreeSet<DefRow>,
    refs: &mut BTreeSet<RefRow>,
    imports: &mut BTreeSet<ImportRow>,
    container_by_fn: &mut BTreeMap<String, String>,
) {
    // This node's own refs carry the ENCLOSING container (its function
    // name-def is contained by the outer scope — matches cmd_parse).
    for r in extract_rust(&node, src, "n", source_id, container) {
        match r {
            ExtractedRef::Def {
                token,
                canonical_kind,
                ..
            } => {
                defs.insert(DefRow {
                    token,
                    source_id: source_id.to_string(),
                    kind: canonical_kind.unwrap_or("?").to_string(),
                });
            }
            ExtractedRef::Ref {
                token,
                qualifier,
                container_node_id,
                ..
            } => {
                refs.insert(RefRow {
                    token,
                    qualifier,
                    container: container_node_id,
                    source_id: source_id.to_string(),
                });
            }
            ExtractedRef::Import { alias, path, .. } => {
                imports.insert(ImportRow {
                    alias,
                    path,
                    source_id: source_id.to_string(),
                });
            }
        }
    }

    // If THIS node is a function, its subtree's container becomes the
    // identity chosen by `keying`. Record name → id so a driver can resolve
    // "review the function named F".
    let own_container: Option<String> = (node.kind() == "function_item")
        .then(|| node.child_by_field_name("name"))
        .flatten()
        .and_then(|n| n.utf8_text(src).ok())
        .map(|name| {
            let id = container_id(keying, node_id, node, src, name);
            container_by_fn.insert(name.to_string(), id.clone());
            id
        });
    let child_container = own_container.as_deref().or(container);

    // Descend, assigning each named child its daemon-style positional
    // node_id (`{kind}` or `{kind}_{idx}` for repeated same-kind siblings).
    let mut counts: HashMap<&str, usize> = HashMap::new();
    {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            *counts.entry(child.kind()).or_insert(0) += 1;
        }
    }
    let mut idx: HashMap<&str, usize> = HashMap::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        let seg = if counts[kind] > 1 {
            let i = idx.entry(kind).or_insert(0);
            let s = format!("{kind}_{i}");
            *i += 1;
            s
        } else {
            kind.to_string()
        };
        let child_id = format!("{node_id}/{seg}");
        walk_extract(
            child,
            src,
            source_id,
            &child_id,
            child_container,
            keying,
            defs,
            refs,
            imports,
            container_by_fn,
        );
    }
}

/// Resolve a function's container identity string under `keying`.
///
/// - `Name` → `fn:{name}` (the original experiment's key).
/// - `Positional` → the daemon's `container_node_id`: the positional AST
///   path. SHIFTS when a function is inserted above `F` (its `_{idx}`
///   sibling suffix moves). Reproduces ADR-0031 caveat #1.
/// - `Stable` → a reflow-invariant node_hash-style identity: the
///   [`signature_node_hash`] of `F`. Position- and body-invariant.
fn container_id(
    keying: ContainerKeying,
    node_id: &str,
    node: Node<'_>,
    src: &[u8],
    name: &str,
) -> String {
    match keying {
        ContainerKeying::Name => format!("fn:{name}"),
        ContainerKeying::Positional => node_id.to_string(),
        ContainerKeying::Stable => stable_container_id(node, src),
    }
}

/// The reflow-invariant, body-invariant stable container identity string
/// (`h:{hex}`) for a `function_item` node — the [`ContainerKeying::Stable`]
/// id, computed position-independently so it is identical to the string
/// [`build_substrate`] threads onto the function's refs. Exposed so the
/// git-replay bench (`f3a81e`) can resolve a specific function region to the
/// container id its refs are grouped under WITHOUT round-tripping through
/// the name-keyed `container_by_fn` map (which collides on same-name
/// functions across a multi-file corpus).
pub fn stable_container_id(func: Node<'_>, src: &[u8]) -> String {
    let h = signature_node_hash(func, src);
    let mut s = String::with_capacity(2 + 64);
    s.push_str("h:");
    for b in h {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// Review-target enumeration for the git-replay superset-correctness bench
// (ADR-0031 caveat #3, bead ley-line-open-055f79 / f3a81e).
// ---------------------------------------------------------------------------

/// One reviewable `function_item` region in a corpus: the alignment key
/// (`source_id`, `name`, `occurrence` in document order within the file),
/// the [`ContainerKeying::Stable`] container id its refs carry (so a driver
/// can call `restriction_for_call_target` / `review_call_targets` for
/// exactly this function), and its byte span within its file's source.
#[derive(Clone, Debug)]
pub struct ReviewTarget {
    pub source_id: String,
    pub name: String,
    pub occurrence: u32,
    pub container: String,
    pub start: usize,
    pub end: usize,
}

fn collect_function_items<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if node.kind() == "function_item" {
        out.push(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_function_items(child, out);
    }
}

/// Enumerate every reviewable function region across a corpus, keyed by
/// stable container identity. Document-order `occurrence` disambiguates a
/// name appearing more than once in a file (mirrors the rung-2 replay's
/// alignment key). Files the grammar refuses are skipped (a half-written
/// file mid-rebase). The `container` string is byte-for-byte the id
/// [`build_substrate`] stores on the same function's refs under the same
/// `keying`, so restriction/review lookups by it hit the right rows.
pub fn review_targets(corpus: &[(String, String)], keying: ContainerKeying) -> Vec<ReviewTarget> {
    let mut out = Vec::new();
    for (path, src) in corpus {
        let Some(tree) = parse_rust(src.as_bytes()) else {
            continue;
        };
        let mut nodes = Vec::new();
        collect_function_items(tree.root_node(), &mut nodes);
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for node in nodes {
            let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src.as_bytes()).ok())
            else {
                continue;
            };
            let occurrence = {
                let c = counts.entry(name.to_string()).or_insert(0);
                let v = *c;
                *c += 1;
                v
            };
            let container = container_id(keying, "", node, src.as_bytes(), name);
            out.push(ReviewTarget {
                source_id: path.clone(),
                name: name.to_string(),
                occurrence,
                container,
                start: node.start_byte(),
                end: node.end_byte(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// node_hash-style stable container identity (ADR-0027 stand-in)
// ---------------------------------------------------------------------------
//
// Reflow-invariant identity for the container, computed the SAME WAY as
// ADR-0027's merkle-AST node_hash (bottom-up hash over canonical kind +
// terminal token, source order; spans / positions OUT), but over F's
// SIGNATURE subtree (function_item minus its `body` block) so it is also
// BODY-invariant.
//
// Body-invariance is required, not incidental: the whole-function
// node_hash changes on any body edit, which would defeat the load-bearing
// body-only true-skips (local-rename, body-arith). Excluding the body
// keeps those skips while still invalidating on a signature change.
//
// Two deliberate stand-in deviations from cmd_parse's production node_hash,
// documented for the f38a86-follow-up integration (see final report):
//   1. SHA-256 (leyline-sheaf's hash), not BLAKE3 — the container id is an
//      opaque string; only the reflow/body invariance matters here.
//   2. raw tree-sitter `node.kind()`, not `TsLanguage::canonical_kind`
//      (κ canonicalization lives in leyline-ts, which production would use).
// The DOMAIN below is distinct from `llo/ast/v1` so a stand-in hash can
// never be mistaken for a production node_hash.

const SIG_HASH_DOMAIN: &[u8] = b"llo/sheaf/restriction-cache/sig/v1";
const SIG_TAG_LEAF: u8 = 0x00;
const SIG_TAG_INTERNAL: u8 = 0x01;

fn sig_write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

fn sig_hash_leaf(kind: &str, token: &str) -> [u8; 32] {
    let mut p = Vec::with_capacity(SIG_HASH_DOMAIN.len() + 8 + kind.len() + token.len());
    p.extend_from_slice(SIG_HASH_DOMAIN);
    p.push(SIG_TAG_LEAF);
    sig_write_uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    sig_write_uvarint(&mut p, token.len() as u64);
    p.extend_from_slice(token.as_bytes());
    Sha256::digest(&p).into()
}

fn sig_hash_internal(kind: &str, child_hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut p =
        Vec::with_capacity(SIG_HASH_DOMAIN.len() + 8 + kind.len() + child_hashes.len() * 32);
    p.extend_from_slice(SIG_HASH_DOMAIN);
    p.push(SIG_TAG_INTERNAL);
    sig_write_uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    sig_write_uvarint(&mut p, child_hashes.len() as u64);
    for h in child_hashes {
        p.extend_from_slice(h);
    }
    Sha256::digest(&p).into()
}

/// Whole-subtree node_hash (kind + token, source order, position-blind).
fn node_hash(node: Node<'_>, src: &[u8]) -> [u8; 32] {
    let mut children: Vec<Node<'_>> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let ch = cursor.node();
            if !ch.is_extra() {
                children.push(ch);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if children.is_empty() {
        let token = node.utf8_text(src).unwrap_or("");
        sig_hash_leaf(node.kind(), token)
    } else {
        let child_hashes: Vec<[u8; 32]> = children.iter().map(|c| node_hash(*c, src)).collect();
        sig_hash_internal(node.kind(), &child_hashes)
    }
}

/// The container's stable identity: `node_hash` of the function's
/// SIGNATURE — its non-extra children in source order, EXCLUDING the `body`
/// field (a.k.a. the `block`). Reflow-invariant (no spans) and
/// body-invariant (no block), collision-resistant across signatures.
fn signature_node_hash(func: Node<'_>, src: &[u8]) -> [u8; 32] {
    let mut child_hashes: Vec<[u8; 32]> = Vec::new();
    let mut cursor = func.walk();
    if cursor.goto_first_child() {
        loop {
            let ch = cursor.node();
            let is_body = cursor.field_name() == Some("body") || ch.kind() == "block";
            if !ch.is_extra() && !is_body {
                child_hashes.push(node_hash(ch, src));
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    sig_hash_internal(func.kind(), &child_hashes)
}

// ===========================================================================
// PUBLIC-API family extraction (bead ley-line-open-0567e8 / f463aa)
//
// Parser-dependent build of the parser-INDEPENDENT `ApiDefRow` — the
// signature-scoped `node_defs` facts a public-API review reads. Populates
// each public function's: visibility, the SIGNATURE stable identity
// (`signature_node_hash`, declaration subtree minus body), and the
// param/return TYPE identifiers the signature names. Body content never
// enters — that is the whole point of the family, and the reason a
// body-only edit (a private-helper rename) leaves the public-API
// restriction unchanged while the call-target restriction would move.
// ===========================================================================

/// Build the `ApiDefRow` for every `function_item` in the corpus, keyed by
/// function name. Parser-dependent stand-in for the daemon reading these
/// fields off its `node_defs` fact table.
pub fn build_api_defs(corpus: &[(String, String)]) -> BTreeMap<String, ApiDefRow> {
    let mut out: BTreeMap<String, ApiDefRow> = BTreeMap::new();
    for (path, src) in corpus {
        let tree = parse_rust(src.as_bytes()).expect("fixture source must parse");
        collect_api_defs(tree.root_node(), src.as_bytes(), path, &mut out);
    }
    out
}

fn collect_api_defs(
    node: Node<'_>,
    src: &[u8],
    source_id: &str,
    out: &mut BTreeMap<String, ApiDefRow>,
) {
    if node.kind() == "function_item"
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(src)
        && !name.is_empty()
    {
        let mut sig_type_tokens: Vec<String> = Vec::new();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_type_tokens(params, src, &mut sig_type_tokens);
        }
        if let Some(ret) = node.child_by_field_name("return_type") {
            collect_type_tokens(ret, src, &mut sig_type_tokens);
        }
        sig_type_tokens.sort();
        sig_type_tokens.dedup();

        out.insert(
            name.to_string(),
            ApiDefRow {
                token: name.to_string(),
                // Free functions in these fixtures carry no receiver; an
                // impl method would carry `Recv` here (the row is
                // keying-agnostic — the field exists, the fixtures don't
                // exercise it).
                qualifier: None,
                kind: leyline_ts::languages::TsLanguage::Rust
                    .canonical_kind(node.kind())
                    .unwrap_or("?")
                    .to_string(),
                source_id: source_id.to_string(),
                is_public: fn_is_public(node, src),
                sig_identity: signature_node_hash(node, src),
                sig_type_tokens,
            },
        );
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_api_defs(child, src, source_id, out);
    }
}

/// `true` iff the function has a `visibility_modifier` child beginning with
/// `pub` (`pub`, `pub(crate)`, `pub(super)`, …). A bare `fn` is private.
fn fn_is_public(func: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = func.walk();
    func.named_children(&mut cursor).any(|ch| {
        ch.kind() == "visibility_modifier"
            && ch
                .utf8_text(src)
                .map(|t| t.starts_with("pub"))
                .unwrap_or(false)
    })
}

/// Collect the TYPE identifiers a signature subtree names — `type_identifier`
/// (`Widget`) and `primitive_type` (`i64`) leaves anywhere under the
/// parameter list or return type (so `&Widget`, `Vec<Widget>`, `Option<i64>`
/// all contribute their inner type names). Deliberately NOT the parameter
/// NAMES (patterns) — a public API is its types, not its binding names.
fn collect_type_tokens(node: Node<'_>, src: &[u8], out: &mut Vec<String>) {
    if matches!(node.kind(), "type_identifier" | "primitive_type")
        && let Ok(t) = node.utf8_text(src)
        && !t.is_empty()
    {
        out.push(t.to_string());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_tokens(child, src, out);
    }
}

// ---------------------------------------------------------------------------
// Parser-dependent baseline policy: the identifier-blind AST-shape hash.
// ---------------------------------------------------------------------------

/// Identifier-blind structural hash: the pre-order named-node kind sequence
/// of every file (ADR-0030 rung 2's representation, exact-hashed). Parser-
/// dependent, so it stays in the extraction layer.
pub fn ast_shape_hash(corpus: &[(String, String)]) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, src) in corpus {
        let tree = parse_rust(src.as_bytes()).expect("fixture source must parse");
        let mut kinds: Vec<&'static str> = Vec::new();
        kind_sequence(tree.root_node(), &mut kinds);
        h.update(format!("{path}{US}{}\n", kinds.join("\u{1f}")).as_bytes());
    }
    h.finalize().into()
}
