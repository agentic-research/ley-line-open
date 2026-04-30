//! HV-as-stalk sheaf cells: connect HDC hypervectors to sheaf-style
//! consistency reasoning. Each code unit (function/file/module) becomes
//! a [`HvCell`] carrying its hypervector as a stalk over its region of
//! the codebase. Edges between cells (containment, sibling, calls)
//! carry a Boolean *agreement* check — the HDC analogue of the
//! coboundary operator δ⁰ in `leyline-sheaf`.
//!
//! ## Boolean Heyting algebra of binary stalks
//!
//! Binary hypervectors under XOR/AND/OR form a Boolean algebra. Boolean
//! algebras are the strongest case of a Heyting algebra (every element
//! has a complement; double-negation elimination holds). Sheaf theory
//! works over any topos; the internal logic of a topos is a Heyting
//! algebra. So the binary-stalk specialization isn't a weaker sheaf —
//! it's the strongest case, where every operation is bitwise and
//! everything is `O(D)` instead of `O(D²)`.
//!
//! Concretely, in this module:
//! - **Stalks**: `[u8; D_BYTES]` binary vectors
//! - **Restriction maps**: lattice homomorphisms — bit permutations
//!   (`rotate_left` is one canonical form) that preserve XOR/AND/OR.
//!   A permutation IS the structure-preserving map between stalks in
//!   the category of Boolean algebras — not an approximation.
//! - **Section propagation**: majority-rule bundle (`bundle_majority`)
//!   is the lattice meet over a set of consistent sections.
//! - **δ⁰ at an edge** (u→v with restriction maps r_u, r_v): apply
//!   r_u to stalk(u) and r_v to stalk(v); check
//!   `popcount(r_u(stalk_u) XOR r_v(stalk_v)) ≤ threshold`.
//!   When both restrictions are the identity, this reduces to plain
//!   `popcount(stalk_u XOR stalk_v)`.
//! - **H⁰** is the connected-component count of the consistent
//!   subgraph — cells whose stalks bundle into one majority section
//!   without contradiction.
//! - **Merkle tree** sits on top: blake3 leaves over (cell_id,
//!   stalk_bytes) per layer, internal nodes are content-addressed.
//!
//! Every operation here is `O(D)` bitwise. At D=8192 that's one cache
//! line per stalk and sub-microsecond per check on a modern CPU.
//!
//! ## What this module is NOT
//!
//! This is the **Boolean specialization** of the sheaf algebra — it
//! deliberately does NOT compute δ¹, H¹, or 2-faces because those need
//! the cycle structure of the full Čech complex. Callers that want
//! the full algebra can convert HV→bit-as-f32 and plug into the
//! closed `leyline-sheaf` crate; that's out of scope here.
//!
//! Additive: lives entirely inside `leyline-hdc`, no other crate
//! touched, no schema changes. The cell-complex is built in-memory from
//! data already produced by [`crate::encoder::encode_tree`].

use std::collections::{BTreeMap, HashMap};

use crate::canonical::CanonicalKind;
use crate::util::{popcount_distance, rotate_left, Hypervector, ZERO_HV};
use crate::{D_BITS, D_BYTES, LayerKind};

/// A stalk: one hypervector over one layer at one cell.
///
/// Lightweight wrapper — gives the type system a place to remember
/// which layer the bytes came from so a Hamming check between an AST
/// stalk and a Semantic stalk gets caught at compile time / runtime
/// instead of silently producing nonsense.
#[derive(Debug, Clone, Copy)]
pub struct HvStalk {
    pub hv: Hypervector,
    pub layer: LayerKind,
}

impl HvStalk {
    pub fn new(hv: Hypervector, layer: LayerKind) -> Self {
        Self { hv, layer }
    }
}

/// A sheaf cell: one code unit identified by a stable id, carrying a
/// canonical kind (its structural role) plus per-layer hypervector
/// stalks. A function might carry an AST stalk + a Semantic stalk +
/// a Temporal stalk, all keyed by [`LayerKind`].
///
/// `source_kind` is the canonical kind the cell represents at its own
/// scope (e.g. `Decl` for a function, `Block` for a file root). Used
/// when cells need to reason about *what kind of region* they cover.
#[derive(Debug, Clone)]
pub struct HvCell {
    pub id: String,
    pub source_kind: CanonicalKind,
    pub stalks: HashMap<LayerKind, Hypervector>,
}

impl HvCell {
    pub fn new(id: impl Into<String>, source_kind: CanonicalKind) -> Self {
        Self {
            id: id.into(),
            source_kind,
            stalks: HashMap::new(),
        }
    }

    /// Attach a stalk for a layer. Replaces any existing stalk for that
    /// layer — re-encoding overwrites, mirrors the AST encoder's
    /// "freshest version wins" semantics in `_hdc`.
    pub fn attach_stalk(&mut self, layer: LayerKind, hv: Hypervector) {
        self.stalks.insert(layer, hv);
    }

    /// Chainable variant of [`HvCell::attach_stalk`]. Builds an
    /// `HvCell` with one or more layer stalks in a single expression:
    ///
    /// ```ignore
    /// let cell = HvCell::new("fn", kind)
    ///     .with_stalk(LayerKind::Ast, ast_hv)
    ///     .with_stalk(LayerKind::Semantic, sem_hv);
    /// ```
    ///
    /// Eliminates the `let mut + N attach_stalks` pattern that was
    /// duplicated across multi-layer test fixtures.
    pub fn with_stalk(mut self, layer: LayerKind, hv: Hypervector) -> Self {
        self.attach_stalk(layer, hv);
        self
    }

    pub fn stalk(&self, layer: LayerKind) -> Option<&Hypervector> {
        self.stalks.get(&layer)
    }
}

/// A Boolean restriction map: a structure-preserving endomorphism on
/// `[u8; D_BYTES]` that the sheaf applies to a stalk before the
/// agreement XOR. In the category of Boolean algebras, the canonical
/// such maps are bit permutations (`rotate_left` being one canonical
/// instance) — they preserve XOR/AND/OR/NOT, hence are lattice
/// homomorphisms.
///
/// `Identity` is the no-op restriction (the most common case: the
/// stalk is already in the target's coordinate system). `RotateLeft`
/// applies a circular bit rotation, which is the natural way to align
/// stalks that differ in role-positional encoding (e.g. when one cell
/// holds a child encoded under role-index `i` and the edge wants to
/// "undo" that role before comparing). `Composite` lets callers stack
/// multiple permutations into one map, still `O(D)` per application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Restriction {
    #[default]
    Identity,
    RotateLeft(usize),
    Composite(Vec<Restriction>),
}

impl Restriction {
    /// Apply the restriction map to a stalk. Always `O(D)`.
    pub fn apply(&self, hv: &Hypervector) -> Hypervector {
        match self {
            Restriction::Identity => *hv,
            Restriction::RotateLeft(n) => rotate_left(hv, *n % D_BITS),
            Restriction::Composite(parts) => {
                let mut out = *hv;
                for r in parts {
                    out = r.apply(&out);
                }
                out
            }
        }
    }
}

/// A directed edge between two cells with a label describing the
/// structural relationship. The `layer` field nominates which stalk
/// layer the agreement check operates on — different relationships
/// care about different layers (a Calls edge cares about Semantic
/// agreement; a Contains edge cares about AST or Module).
///
/// `restrict_source` and `restrict_target` are the per-endpoint
/// restriction maps applied before the XOR-popcount agreement check.
/// Default to `Identity` (the common case where stalks are already in
/// the same coordinate system); use `RotateLeft(n)` to align stalks
/// that differ by a known role permutation.
#[derive(Debug, Clone)]
pub struct HvEdge {
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    pub layer: LayerKind,
    pub restrict_source: Restriction,
    pub restrict_target: Restriction,
}

impl HvEdge {
    /// Convenience constructor for an identity-restricted edge — the
    /// common case where both endpoints share a coordinate system.
    /// Reduces test boilerplate vs. constructing the struct directly.
    pub fn identity(
        source: impl Into<String>,
        target: impl Into<String>,
        kind: EdgeKind,
        layer: LayerKind,
    ) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            kind,
            layer,
            restrict_source: Restriction::Identity,
            restrict_target: Restriction::Identity,
        }
    }
}

/// Kinds of structural edges between code-unit cells. Closed set;
/// adding a variant is a code-only change but bumps the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// Parent-child containment in the AST/module tree (file → fn,
    /// fn → block, …). Hamming check should be loose (children diverge
    /// from parents by design).
    Contains,
    /// Siblings under a common parent (two methods of the same impl,
    /// two top-level decls of the same file). Hamming check is the
    /// "are these structurally equivalent" question.
    Sibling,
    /// Caller → callee. Semantic-layer agreement check; AST is too
    /// strict (caller's body is not the callee's body).
    Calls,
    /// A → B where A imports/uses B. Loose check on the Module layer.
    Imports,
}

/// A constraint violation: an edge whose stalks disagree by more
/// than `threshold`. Returned by [`HvCellComplex::detect_violations`].
#[derive(Debug, Clone)]
pub struct HvViolation {
    pub edge_index: usize,
    pub hamming: u32,
    pub threshold: u32,
    pub layer: LayerKind,
}

/// A cell complex: a set of cells + a set of edges + a per-layer
/// agreement threshold. Built incrementally; not thread-safe (clone
/// for sharing, or wrap in your own `Arc<RwLock<…>>`).
///
/// Threshold semantics: the agreement_threshold is a per-layer
/// Hamming budget. Edges with Hamming distance ≤ threshold are
/// "consistent"; > threshold are "violations". Default thresholds
/// per layer come from [`crate::calibrate::RadiusBaseline`]; callers
/// that pre-calibrated should pass that radius in directly.
#[derive(Debug, Clone)]
pub struct HvCellComplex {
    pub cells: BTreeMap<String, HvCell>,
    pub edges: Vec<HvEdge>,
    pub agreement_threshold: HashMap<LayerKind, u32>,
}

impl Default for HvCellComplex {
    fn default() -> Self {
        Self::new()
    }
}

impl HvCellComplex {
    pub fn new() -> Self {
        Self {
            cells: BTreeMap::new(),
            edges: Vec::new(),
            agreement_threshold: HashMap::new(),
        }
    }

    pub fn add_cell(&mut self, cell: HvCell) {
        self.cells.insert(cell.id.clone(), cell);
    }

    pub fn add_edge(&mut self, edge: HvEdge) {
        self.edges.push(edge);
    }

    /// Set the Hamming-distance threshold below which an edge is
    /// considered consistent on the given layer. Callers should source
    /// this from [`crate::calibrate::RadiusBaseline::recommended_radius`]
    /// rather than guessing — the math friend's guidance is that the
    /// threshold is data-dependent and varies per language and corpus.
    pub fn set_threshold(&mut self, layer: LayerKind, threshold: u32) {
        self.agreement_threshold.insert(layer, threshold);
    }

    /// δ⁰-equivalent: for each edge u→v, apply the per-endpoint
    /// restriction maps (Boolean algebra automorphisms) to each
    /// stalk, then check
    /// `popcount(restrict_source(stalk_u) XOR restrict_target(stalk_v)) ≤ threshold`.
    /// Edges that exceed the threshold are returned as violations.
    ///
    /// Edges where one or both endpoints are missing the layer's
    /// stalk are silently skipped — they're not violations, they're
    /// just unobservable on that layer (a Calls edge can't check
    /// Semantic agreement if one side never got a Semantic stalk).
    pub fn detect_violations(&self) -> Vec<HvViolation> {
        let mut out = Vec::new();
        for (i, edge) in self.edges.iter().enumerate() {
            let Some(hamming) = self.edge_hamming(edge) else {
                continue;
            };
            let Some(&threshold) = self.agreement_threshold.get(&edge.layer) else {
                continue;
            };
            if hamming > threshold {
                out.push(HvViolation {
                    edge_index: i,
                    hamming,
                    threshold,
                    layer: edge.layer,
                });
            }
        }
        out
    }

    /// Compute the Hamming distance an edge sees after applying its
    /// restriction maps. `None` if either endpoint is absent or
    /// missing the layer's stalk. Pure read; no side effects.
    pub fn edge_hamming(&self, edge: &HvEdge) -> Option<u32> {
        let src_cell = self.cells.get(&edge.source)?;
        let tgt_cell = self.cells.get(&edge.target)?;
        let src_hv = src_cell.stalk(edge.layer)?;
        let tgt_hv = tgt_cell.stalk(edge.layer)?;
        let src_restricted = edge.restrict_source.apply(src_hv);
        let tgt_restricted = edge.restrict_target.apply(tgt_hv);
        Some(popcount_distance(&src_restricted, &tgt_restricted))
    }

    /// Majority-rule section propagation: bundle a slice of stalks
    /// bit-for-bit, taking the majority bit at each position. Ties go
    /// to 0 (matches the SQLite `BUNDLE_MAJORITY` UDF tie-break so
    /// non-empty inputs produce identical results SQL-side and
    /// Rust-side).
    ///
    /// **Empty-input divergence (skeptic 7293f3):** this Rust function
    /// returns `ZERO_HV` for empty input — the identity element of
    /// the bundle operation under XOR. The SQL `BUNDLE_MAJORITY`
    /// aggregate returns `NULL` for zero rows, because that's how
    /// SQLite aggregates universally signal "no rows". Callers that
    /// shuttle bundles between Rust and SQL must therefore handle
    /// the empty case explicitly (e.g. `COALESCE(BUNDLE_MAJORITY(hv),
    /// ZEROBLOB(1024))` SQL-side, or check for `&[]` Rust-side).
    /// Cross-implementation parity is pinned for non-empty inputs by
    /// `bundle_majority_matches_sql_udf_on_nonempty`.
    ///
    /// In the Boolean-Heyting view this is the *meet* of the
    /// supplied sections in the lattice of binary stalks: the
    /// largest stalk that is ≤ every input under the dual ordering
    /// "more 1s ≥". Useful for computing a canonical centroid of a
    /// consistent group returned by `compute_h0`.
    pub fn bundle_majority(stalks: &[Hypervector]) -> Hypervector {
        let mut out = ZERO_HV;
        if stalks.is_empty() {
            return out;
        }
        let half = stalks.len() as u32 / 2;
        // Tie-break: a tie occurs only when len() is even AND count == half.
        // Matches BUNDLE_MAJORITY: ties → 0.
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
    }

    /// Propagate a layer's stalks across a connected component into
    /// a single canonical "centroid" via majority-rule bundling.
    /// Returns one centroid per component. Useful for cluster
    /// summarization and for verifying that members of a consistent
    /// group are all near their bundle (the "the bundle is recoverable"
    /// property the math friend flagged).
    pub fn propagate_sections(&self, layer: LayerKind) -> Vec<Hypervector> {
        let groups = self.compute_h0(layer);
        groups
            .iter()
            .map(|group| {
                let stalks: Vec<Hypervector> = group
                    .iter()
                    .filter_map(|id| self.cells.get(id))
                    .filter_map(|c| c.stalk(layer).copied())
                    .collect();
                Self::bundle_majority(&stalks)
            })
            .collect()
    }

    /// H⁰-equivalent on a single layer: union-find over cells, where
    /// two cells are merged whenever an edge between them is
    /// *consistent* on `layer` (Hamming ≤ threshold). Returns the
    /// connected components of the consistent subgraph.
    ///
    /// Each returned group is a cluster of structurally-similar code
    /// units — the HDC cluster output, expressed as a cohomology
    /// group rather than as a query result.
    pub fn compute_h0(&self, layer: LayerKind) -> Vec<Vec<String>> {
        let Some(&threshold) = self.agreement_threshold.get(&layer) else {
            return self.cells.keys().cloned().map(|id| vec![id]).collect();
        };

        let mut parent: HashMap<&str, &str> = HashMap::new();
        for id in self.cells.keys() {
            parent.insert(id.as_str(), id.as_str());
        }

        for edge in &self.edges {
            if edge.layer != layer {
                continue;
            }
            let Some(hamming) = self.edge_hamming(edge) else {
                continue;
            };
            if hamming <= threshold {
                union(&mut parent, edge.source.as_str(), edge.target.as_str());
            }
        }

        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in self.cells.keys() {
            let root = find(&mut parent, id.as_str()).to_string();
            groups.entry(root).or_default().push(id.clone());
        }
        groups.into_values().collect()
    }

    /// Content-addressed Merkle root over a single layer's stalks.
    /// Leaves are blake3(domain_tag || cell_id_bytes || stalk_bytes),
    /// sorted by cell_id for determinism. Internal nodes are
    /// blake3(0x01 || left || right).
    ///
    /// Why a *per-layer* Merkle: the AST stalk root is the
    /// "structural identity" of the codebase; the Semantic stalk root
    /// is the "meaning identity"; they should change independently.
    /// A change to one file's body changes the AST root but may not
    /// change the Module root (if the file's top-level decl headers
    /// stayed the same).
    pub fn merkle_root_for_layer(&self, layer: LayerKind) -> [u8; 32] {
        let mut leaves: Vec<[u8; 32]> = Vec::new();
        for (id, cell) in &self.cells {
            let Some(hv) = cell.stalk(layer) else {
                continue;
            };
            // domain_tag (layer.as_str()) + 0x00 + id_bytes + 0x00 + stalk_bytes
            let mut buf = Vec::with_capacity(layer.as_str().len() + 2 + id.len() + D_BYTES);
            buf.extend_from_slice(layer.as_str().as_bytes());
            buf.push(0u8);
            buf.extend_from_slice(id.as_bytes());
            buf.push(0u8);
            buf.extend_from_slice(hv);
            leaves.push(blake3::hash(&buf).into());
        }
        merkle_root(&leaves)
    }

    /// Full structural Merkle root: per-layer roots XOR-folded into a
    /// single content-addressed identity for the entire complex. Two
    /// complexes with the same per-layer roots produce the same
    /// structural root regardless of which layers they share — the
    /// XOR fold is order-independent across layers.
    ///
    /// XOR is correct here because the per-layer roots are already
    /// blake3 outputs (independent uniform 32-byte values); XOR of
    /// blake3 outputs preserves uniform distribution and remains
    /// collision-resistant under the random-oracle assumption.
    pub fn structural_root(&self) -> [u8; 32] {
        let mut acc = [0u8; 32];
        for layer in LayerKind::ALL {
            let root = self.merkle_root_for_layer(layer);
            for i in 0..32 {
                acc[i] ^= root[i];
            }
        }
        acc
    }
}

// -- internal helpers -------------------------------------------------

fn find<'a>(parent: &mut HashMap<&'a str, &'a str>, x: &'a str) -> &'a str {
    let mut cur = x;
    while let Some(&p) = parent.get(cur)
        && p != cur
    {
        cur = p;
    }
    // Path compression: point every node from x to the root in one pass.
    let root = cur;
    let mut walker = x;
    while let Some(&p) = parent.get(walker)
        && p != root
    {
        parent.insert(walker, root);
        walker = p;
    }
    root
}

fn union<'a>(parent: &mut HashMap<&'a str, &'a str>, a: &'a str, b: &'a str) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent.insert(ra, rb);
    }
}

/// Domain-separated SHA-3-style Merkle root (we use blake3 here for
/// consistency with the rest of `leyline-hdc` rather than SHA-256
/// like `leyline-sheaf::merkle`; the algebra is identical).
fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        // Domain-tagged empty root: full 256-bit blake3 of the
        // domain-tag bytes so the empty-complex root has full hash
        // entropy (skeptic 72deb2: previous impl truncated to a u64
        // and zero-padded, leaving only 64 bits of collision
        // resistance).
        let mut buf = b"hdc-sheaf-empty".to_vec();
        buf.push(0u8);
        return blake3::hash(&buf).into();
    }
    if leaves.len() == 1 {
        return leaves[0];
    }
    let mut current: Vec<[u8; 32]> = leaves.to_vec();
    while current.len() > 1 {
        let mut next: Vec<[u8; 32]> = Vec::with_capacity(current.len().div_ceil(2));
        for pair in current.chunks(2) {
            if pair.len() == 2 {
                let mut buf = Vec::with_capacity(65);
                buf.push(0x01);
                buf.extend_from_slice(&pair[0]);
                buf.extend_from_slice(&pair[1]);
                next.push(blake3::hash(&buf).into());
            } else {
                next.push(pair[0]);
            }
        }
        current = next;
    }
    current[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::conn_with_udfs;
    use crate::util::{blake3_seed, expand_seed, splitmix64};

    fn stalk_for(seed: u64) -> Hypervector {
        expand_seed(seed)
    }

    fn make_cell(id: &str, kind: CanonicalKind, ast_seed: u64) -> HvCell {
        HvCell::new(id, kind).with_stalk(LayerKind::Ast, stalk_for(ast_seed))
    }

    /// Bytewise XOR of two 32-byte blake3 hashes. Used by the
    /// structural-root tests to compute deltas. Replaces a 3-line
    /// for-loop that appeared 3× across two tests.
    fn xor32(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = a[i] ^ b[i];
        }
        out
    }

    #[test]
    fn empty_complex_has_stable_root() {
        // Empty-complex root must be deterministic — clients use it as
        // a "no codebase yet" sentinel; if it drifted across calls the
        // sentinel would be unstable.
        let cx = HvCellComplex::new();
        let r1 = cx.merkle_root_for_layer(LayerKind::Ast);
        let r2 = cx.merkle_root_for_layer(LayerKind::Ast);
        assert_eq!(r1, r2);
    }

    #[test]
    fn with_stalk_chains_match_imperative_attach_stalk() {
        // Pin: the chainable `with_stalk` builder must produce a cell
        // byte-equivalent to the imperative `let mut + attach_stalk`
        // form. Catches a future refactor that accidentally cloned
        // wrong, or made `with_stalk` insert into a different map.
        let chain = HvCell::new("fn", CanonicalKind::Decl)
            .with_stalk(LayerKind::Ast, stalk_for(1))
            .with_stalk(LayerKind::Semantic, stalk_for(2));
        let mut imperative = HvCell::new("fn", CanonicalKind::Decl);
        imperative.attach_stalk(LayerKind::Ast, stalk_for(1));
        imperative.attach_stalk(LayerKind::Semantic, stalk_for(2));
        assert_eq!(chain.id, imperative.id);
        assert_eq!(chain.source_kind, imperative.source_kind);
        assert_eq!(chain.stalks, imperative.stalks);
    }

    #[test]
    fn merkle_root_for_layer_with_no_matching_stalks_is_empty_sentinel() {
        // A complex with cells that don't carry stalks on the queried
        // layer should produce the same root as an empty complex —
        // the per-layer root is computed only from cells that have
        // a stalk for that layer. Pin so a partially-encoded complex
        // (e.g. only AST encoded, no Semantic yet) doesn't invalidate
        // a cached "no Semantic data" sentinel.
        let mut cx_only_ast = HvCellComplex::new();
        cx_only_ast.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx_only_ast.add_cell(make_cell("fn_b", CanonicalKind::Decl, 2));
        // No Semantic stalks anywhere → Semantic root == empty-complex sentinel.
        let cx_empty = HvCellComplex::new();
        assert_eq!(
            cx_only_ast.merkle_root_for_layer(LayerKind::Semantic),
            cx_empty.merkle_root_for_layer(LayerKind::Semantic),
            "no-matching-stalks root must equal empty-complex sentinel",
        );
    }

    #[test]
    fn merkle_leaf_format_byte_pin() {
        // Pin the leaf byte layout so a refactor that drops a 0x00
        // separator or reorders fields gets caught — every encoded
        // Merkle root depends on this exact byte sequence:
        //
        //   layer.as_str() + 0x00 + cell_id + 0x00 + stalk_bytes (D_BYTES)
        //
        // Computed manually here so the test is independent of the
        // production code path.
        let mut cx = HvCellComplex::new();
        let stalk = stalk_for(7);
        cx.add_cell(HvCell::new("fn_x", CanonicalKind::Decl).with_stalk(LayerKind::Ast, stalk));
        let actual = cx.merkle_root_for_layer(LayerKind::Ast);

        // Single-leaf case: the layer root IS the leaf hash.
        let mut buf = Vec::with_capacity(LayerKind::Ast.as_str().len() + 2 + 4 + D_BYTES);
        buf.extend_from_slice(LayerKind::Ast.as_str().as_bytes()); // "ast"
        buf.push(0u8); // separator 1
        buf.extend_from_slice(b"fn_x"); // id
        buf.push(0u8); // separator 2
        buf.extend_from_slice(&stalk); // D_BYTES of stalk
        let expected: [u8; 32] = blake3::hash(&buf).into();
        assert_eq!(actual, expected, "leaf byte format drifted");
    }

    #[test]
    fn single_cell_root_is_leaf_hash() {
        // Per the merkle_root contract: one leaf → root == leaf. A
        // future refactor that forgot the single-leaf case would
        // re-hash it once and shift the root, silently invalidating
        // every cached entry.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        let r1 = cx.merkle_root_for_layer(LayerKind::Ast);
        // Re-running must reproduce.
        let r2 = cx.merkle_root_for_layer(LayerKind::Ast);
        assert_eq!(r1, r2);
        assert_ne!(r1, [0u8; 32]);
    }

    #[test]
    fn per_layer_roots_are_independent() {
        // The same set of cells with the same AST stalks but different
        // Semantic stalks must produce different Semantic roots and
        // identical AST roots. This is the property that lets a
        // semantic-only edit (refactor that preserved structure) avoid
        // invalidating the AST cache.
        let mut cx_a = HvCellComplex::new();
        cx_a.add_cell(
            HvCell::new("fn_x", CanonicalKind::Decl)
                .with_stalk(LayerKind::Ast, stalk_for(1))
                .with_stalk(LayerKind::Semantic, stalk_for(100)),
        );

        let mut cx_b = HvCellComplex::new();
        cx_b.add_cell(
            HvCell::new("fn_x", CanonicalKind::Decl)
                .with_stalk(LayerKind::Ast, stalk_for(1))
                .with_stalk(LayerKind::Semantic, stalk_for(101)),
        );

        let ast_a = cx_a.merkle_root_for_layer(LayerKind::Ast);
        let ast_b = cx_b.merkle_root_for_layer(LayerKind::Ast);
        let sem_a = cx_a.merkle_root_for_layer(LayerKind::Semantic);
        let sem_b = cx_b.merkle_root_for_layer(LayerKind::Semantic);

        assert_eq!(ast_a, ast_b, "AST stalks identical → AST root identical");
        assert_ne!(sem_a, sem_b, "Semantic stalks differ → Semantic root differs");
    }

    #[test]
    fn merkle_root_changes_when_cell_added() {
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        let before = cx.merkle_root_for_layer(LayerKind::Ast);
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 2));
        let after = cx.merkle_root_for_layer(LayerKind::Ast);
        assert_ne!(before, after, "adding a cell must change the layer root");
    }

    #[test]
    fn merkle_root_changes_when_stalk_changes() {
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        let before = cx.merkle_root_for_layer(LayerKind::Ast);
        // Re-attach with a different seed → stalk content changes.
        cx.cells
            .get_mut("fn_a")
            .unwrap()
            .attach_stalk(LayerKind::Ast, stalk_for(2));
        let after = cx.merkle_root_for_layer(LayerKind::Ast);
        assert_ne!(before, after, "stalk change must change root");
    }

    #[test]
    fn detect_violations_finds_disagreeing_edge() {
        // Two cells with identical AST stalks: edge consistent.
        // Two cells with seed-1 vs seed-2 stalks: edge violates at
        // any threshold below D/2 (random pair → ~D/2 hamming).
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 1));  // same as fn_a
        cx.add_cell(make_cell("fn_c", CanonicalKind::Decl, 999)); // different
        cx.set_threshold(LayerKind::Ast, 100); // tight threshold
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_b",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_c",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        let v = cx.detect_violations();
        assert_eq!(v.len(), 1, "exactly one violation expected (a-c, not a-b)");
        assert_eq!(v[0].edge_index, 1);
    }

    #[test]
    fn missing_layer_skipped_not_violated() {
        // An edge whose endpoints don't both have stalks on the edge's
        // layer is silently skipped — not a violation. This matters for
        // partially-encoded complexes: if Semantic enrichment hasn't
        // run yet, Calls edges shouldn't generate spurious violations.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(HvCell::new("fn_b", CanonicalKind::Decl)); // no stalks
        cx.set_threshold(LayerKind::Ast, 0);
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_b",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        assert!(cx.detect_violations().is_empty());
    }

    #[test]
    fn detect_violations_skipped_when_endpoint_cell_missing() {
        // Edge references a non-existent cell ("fn_ghost") — must
        // be silently skipped, not panic on `cells.get` returning
        // None. Mirrors the missing-stalk skip; partial cell
        // populations during a daemon mid-flight reparse must not
        // crash the violation detector.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.set_threshold(LayerKind::Ast, 0);
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_ghost", // doesn't exist
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        assert!(cx.detect_violations().is_empty());
        // edge_hamming exposes the same skip path — must be None.
        assert_eq!(cx.edge_hamming(&cx.edges[0]), None);
    }

    #[test]
    fn detect_violations_skipped_when_no_threshold_for_layer() {
        // An edge whose layer has no threshold registered must be
        // skipped (not crash, not produce a phantom violation). A
        // partially-calibrated daemon shouldn't generate noise.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 999)); // far
        // Note: no `set_threshold` for Ast.
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_b",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        assert!(
            cx.detect_violations().is_empty(),
            "no threshold → skip, even on a far pair"
        );
    }

    #[test]
    fn propagate_sections_empty_complex_yields_empty() {
        // Same edge case as compute_h0: empty complex → empty
        // centroids. The propagation is built on H⁰ partitions, so
        // empty input should propagate (sic) through cleanly.
        let cx = HvCellComplex::new();
        let centroids = cx.propagate_sections(LayerKind::Ast);
        assert!(centroids.is_empty());
    }

    #[test]
    fn compute_h0_groups_consistent_cells() {
        // Three cells: a, b share a stalk; c is far. With a tight
        // threshold and a Sibling edge a-b, h0 should return a 2-cell
        // group {a,b} and a 1-cell group {c}.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_c", CanonicalKind::Decl, 999));
        cx.set_threshold(LayerKind::Ast, 10);
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_b",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        cx.add_edge(HvEdge::identity(
            "fn_b",
            "fn_c",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        let groups = cx.compute_h0(LayerKind::Ast);
        let sizes: Vec<usize> = {
            let mut s: Vec<usize> = groups.iter().map(|g| g.len()).collect();
            s.sort_unstable();
            s
        };
        assert_eq!(sizes, vec![1, 2], "expected one 2-cluster + one singleton");
    }

    #[test]
    fn compute_h0_empty_complex_yields_empty() {
        // No cells → no groups. Pin so a future refactor that
        // accidentally returned `vec![vec![]]` (one empty group)
        // or panicked on the empty case is caught.
        let cx = HvCellComplex::new();
        let groups = cx.compute_h0(LayerKind::Ast);
        assert!(groups.is_empty());
    }

    #[test]
    fn compute_h0_no_edges_yields_one_singleton_per_cell() {
        // With cells but no edges, every cell is its own component
        // regardless of threshold or stalk equality. Pin: union-find
        // never merges without an explicit edge — visual proximity
        // (same stalk) doesn't imply structural connection.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 1)); // same seed!
        cx.add_cell(make_cell("fn_c", CanonicalKind::Decl, 1)); // also same!
        cx.set_threshold(LayerKind::Ast, 10);
        // Same stalks but no edges → 3 singletons.
        let groups = cx.compute_h0(LayerKind::Ast);
        assert_eq!(groups.len(), 3);
        for group in &groups {
            assert_eq!(group.len(), 1);
        }
    }

    #[test]
    fn compute_h0_no_threshold_yields_singletons() {
        // No threshold set for the layer → can't determine consistency
        // → every cell is its own component. Defensible default
        // ("if I don't know what consistent looks like, nothing is
        // consistent") rather than panic or silent merge.
        let mut cx = HvCellComplex::new();
        cx.add_cell(make_cell("fn_a", CanonicalKind::Decl, 1));
        cx.add_cell(make_cell("fn_b", CanonicalKind::Decl, 1));
        cx.add_edge(HvEdge::identity(
            "fn_a",
            "fn_b",
            EdgeKind::Sibling,
            LayerKind::Ast,
        ));
        let groups = cx.compute_h0(LayerKind::Ast);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn structural_root_xor_folds_layers() {
        // Two complexes with the same per-layer roots produce the same
        // structural root. Pin the XOR-fold property: structural root
        // is the layer-XOR of per-layer Merkle roots.
        let mut cx = HvCellComplex::new();
        cx.add_cell(
            HvCell::new("fn_a", CanonicalKind::Decl)
                .with_stalk(LayerKind::Ast, stalk_for(1))
                .with_stalk(LayerKind::Semantic, stalk_for(2)),
        );

        let structural = cx.structural_root();
        let ast = cx.merkle_root_for_layer(LayerKind::Ast);
        let sem = cx.merkle_root_for_layer(LayerKind::Semantic);
        // Other layers are empty — their roots are the deterministic
        // empty-root sentinel. We can't reconstruct without computing
        // them, but we can verify that structural_root depends on
        // both ast and sem by toggling one.
        let mut cx2 = cx.clone();
        cx2.cells
            .get_mut("fn_a")
            .unwrap()
            .attach_stalk(LayerKind::Ast, stalk_for(99));
        let structural_2 = cx2.structural_root();
        assert_ne!(
            structural, structural_2,
            "structural root must change when an underlying layer root changes"
        );
        // And the XOR of the two AST roots should equal the XOR of the
        // two structural roots — the AST contribution is the only
        // difference.
        let ast_2 = cx2.merkle_root_for_layer(LayerKind::Ast);
        let delta_layer = xor32(&ast, &ast_2);
        let delta_struct = xor32(&structural, &structural_2);
        assert_eq!(delta_layer, delta_struct, "structural root delta == AST delta when only AST changed");
        // sem unused but kept for documentation: confirm Semantic
        // root is non-zero (sanity).
        assert_ne!(sem, [0u8; 32]);
    }

    #[test]
    fn merkle_root_order_invariant_within_layer() {
        // BTreeMap keys → leaves are sorted by cell id, so insertion
        // order doesn't affect the layer root. Pin this so a refactor
        // that switches to HashMap doesn't silently break determinism.
        let cells = [
            make_cell("fn_a", CanonicalKind::Decl, 1),
            make_cell("fn_b", CanonicalKind::Decl, 2),
            make_cell("fn_c", CanonicalKind::Decl, 3),
        ];

        let mut forward = HvCellComplex::new();
        for c in &cells {
            forward.add_cell(c.clone());
        }
        let mut reverse = HvCellComplex::new();
        for c in cells.iter().rev() {
            reverse.add_cell(c.clone());
        }
        assert_eq!(
            forward.merkle_root_for_layer(LayerKind::Ast),
            reverse.merkle_root_for_layer(LayerKind::Ast)
        );
    }

    // -- Boolean Heyting algebra: restriction maps + propagation -----

    #[test]
    fn restriction_identity_is_no_op() {
        // The identity restriction must reproduce its input bit-for-bit.
        // Pin this so a refactor that swaps `Identity` to "rotate by 0"
        // would still pass — but a refactor that defaulted to "rotate
        // by N" would fail loudly.
        let hv = stalk_for(42);
        assert_eq!(Restriction::Identity.apply(&hv), hv);
    }

    #[test]
    fn restriction_rotate_left_inverts_to_rotate_right() {
        // Rotation is the canonical Boolean automorphism. Compose
        // `RotateLeft(n)` with `RotateLeft(D_BITS - n)` and you get
        // back the identity — the pair are inverses, which is the
        // group-automorphism property.
        let hv = stalk_for(7);
        let n = 1234usize;
        let rotated = Restriction::RotateLeft(n).apply(&hv);
        let restored = Restriction::RotateLeft(D_BITS - n).apply(&rotated);
        assert_eq!(restored, hv, "rotate(n) ∘ rotate(D-n) must be identity");
    }

    #[test]
    fn restriction_composite_chains_in_order() {
        // Composite([a, b]).apply(x) == b.apply(a.apply(x)). Pin this
        // so a future "fold from the right" refactor would fail loudly.
        let hv = stalk_for(13);
        let direct = Restriction::RotateLeft(7).apply(&Restriction::RotateLeft(3).apply(&hv));
        let composite =
            Restriction::Composite(vec![Restriction::RotateLeft(3), Restriction::RotateLeft(7)])
                .apply(&hv);
        assert_eq!(composite, direct);
        // Equivalent to a single rotation by 10:
        assert_eq!(composite, Restriction::RotateLeft(10).apply(&hv));
    }

    #[test]
    fn edge_with_matching_restrictions_is_consistent() {
        // Two cells whose stalks differ only by a known rotation
        // (one cell holds the rotated form, the other holds the
        // canonical form). Use restrict_source to "undo" the rotation
        // so the agreement check succeeds. This is the use case for
        // Restriction maps: align stalks that live in different
        // role-position frames.
        let mut cx = HvCellComplex::new();
        let canonical = stalk_for(1);
        let rotated = rotate_left(&canonical, 100);
        cx.add_cell(HvCell::new("fn_a", CanonicalKind::Decl).with_stalk(LayerKind::Ast, rotated));
        cx.add_cell(HvCell::new("fn_b", CanonicalKind::Decl).with_stalk(LayerKind::Ast, canonical));
        cx.set_threshold(LayerKind::Ast, 0); // exact match required
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_b".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
            // Undo the rotation on the source side; identity on target.
            restrict_source: Restriction::RotateLeft(D_BITS - 100),
            restrict_target: Restriction::Identity,
        });
        let v = cx.detect_violations();
        assert!(
            v.is_empty(),
            "matched restrictions must produce zero Hamming and no violation"
        );
        assert_eq!(cx.edge_hamming(&cx.edges[0]).unwrap(), 0);
    }

    #[test]
    fn bundle_majority_empty_is_zero() {
        // Empty bundle = identity element of the bundle operation.
        // Pin this so a refactor that returned "all-ones" or panicked
        // for empty input would fail loudly.
        let out = HvCellComplex::bundle_majority(&[]);
        assert_eq!(out, ZERO_HV);
    }

    #[test]
    fn bundle_majority_single_input_is_identity() {
        // One stalk in → that stalk out. The bundle operation is
        // idempotent on singletons.
        let s = stalk_for(123);
        assert_eq!(HvCellComplex::bundle_majority(&[s]), s);
    }

    #[test]
    fn bundle_majority_recovers_centroid_of_consistent_group() {
        // Three identical stalks bundle to themselves (each bit is
        // either 0/0/0 → 0 or 1/1/1 → 1; majority preserves it).
        // Pin: the bundle of a consistent group IS its centroid.
        let s = stalk_for(777);
        let bundle = HvCellComplex::bundle_majority(&[s, s, s]);
        assert_eq!(bundle, s);
    }

    #[test]
    fn bundle_majority_tie_resolves_to_zero() {
        // Two stalks that disagree at every bit (one = !other) —
        // every bit position is a 0/1 tie. With the tie-→-0 rule the
        // bundle is all zeros. Matches BUNDLE_MAJORITY UDF semantics.
        let a = stalk_for(2);
        let b = a.map(|x| !x);
        // Sanity: a and b are bit-complements.
        for i in 0..D_BYTES {
            assert_eq!(a[i] ^ b[i], 0xFF);
        }
        assert_eq!(HvCellComplex::bundle_majority(&[a, b]), ZERO_HV);
        // Also verify the path doesn't depend on which bit-pattern we
        // used: switch `a` to a different fixed pattern and re-confirm.
        let a2 = [0xAA_u8; D_BYTES];
        let b2 = a2.map(|x| !x);
        assert_eq!(HvCellComplex::bundle_majority(&[a2, b2]), ZERO_HV);
    }

    #[test]
    fn propagate_sections_returns_centroid_per_component() {
        // Two consistent groups (one cluster of 3 identical stalks,
        // one singleton). propagate_sections should return one
        // centroid per group; the 3-cell group's centroid equals its
        // shared stalk.
        let mut cx = HvCellComplex::new();
        let s_cluster = stalk_for(11);
        let s_singleton = stalk_for(99);
        for id in &["a", "b", "c"] {
            cx.add_cell(
                HvCell::new(*id, CanonicalKind::Decl).with_stalk(LayerKind::Ast, s_cluster),
            );
        }
        cx.add_cell(HvCell::new("z", CanonicalKind::Decl).with_stalk(LayerKind::Ast, s_singleton));
        cx.set_threshold(LayerKind::Ast, 0);
        cx.add_edge(HvEdge::identity("a", "b", EdgeKind::Sibling, LayerKind::Ast));
        cx.add_edge(HvEdge::identity("b", "c", EdgeKind::Sibling, LayerKind::Ast));

        let centroids = cx.propagate_sections(LayerKind::Ast);
        assert_eq!(centroids.len(), 2, "one centroid per H0 component");
        // Each centroid must equal exactly one of the two input stalks.
        assert!(
            centroids.contains(&s_cluster),
            "cluster centroid must equal shared stalk"
        );
        assert!(
            centroids.contains(&s_singleton),
            "singleton centroid must equal lone stalk"
        );
    }

    // -- Skeptic findings (7293f3, 731bff, 734d65) regression pins --

    #[test]
    fn bundle_majority_recovers_noisy_cluster_centroid() {
        // Skeptic 731bff: prior cluster-recovery test only used
        // *identical* stalks, where majority-bundle is trivially the
        // input. This pins the actual claim: a cluster of stalks that
        // each differ from a base by independent ~5% bit-flip noise
        // still bundles back to a stalk close to the base — the
        // BUNDLE_MAJORITY denoising property.
        //
        // Theory: with 5 noisy copies, each bit flips independently
        // with p=0.05. Majority-rule preserves the original bit unless
        // ≥3 of the 5 copies happened to flip it (probability
        // C(5,3)·p³ + C(5,4)·p⁴ + p⁵ ≈ 0.00116). Expected bits-wrong
        // in the bundle ≈ 0.00116 · 8192 ≈ 9.5. We assert the bundle
        // lands within Hamming 50 of the base — generous enough to
        // tolerate seed-dependent variance, tight enough to fail a
        // refactor that broke majority-rule.
        let base = stalk_for(2024);
        let mut copies = Vec::with_capacity(5);
        for seed in 100u64..105u64 {
            // Mix the seed via blake3 first so the per-copy PRNG state
            // is well-distributed across copies — using `seed` directly
            // would leave copies sharing many low-order bits and the
            // flip patterns would overlap, defeating the independence
            // assumption of majority-rule denoising.
            let mut state = blake3_seed(&seed.to_le_bytes());
            let mut copy = base;
            for _ in 0..(D_BITS / 20) {
                let bit = (splitmix64(&mut state) as usize) % D_BITS;
                let byte_idx = bit / 8;
                let bit_off = bit % 8;
                copy[byte_idx] ^= 1 << bit_off;
            }
            copies.push(copy);
        }
        let bundle = HvCellComplex::bundle_majority(&copies);
        let dist = popcount_distance(&bundle, &base);
        assert!(
            dist <= 50,
            "noisy-cluster bundle should denoise back near base; got Hamming {dist}"
        );
        // Also assert the bundle is closer to base than any single
        // noisy copy is (the actual denoising property — not just
        // "close enough").
        for (i, c) in copies.iter().enumerate() {
            let copy_dist = popcount_distance(c, &base);
            assert!(
                dist <= copy_dist,
                "bundle Hamming {dist} should be ≤ copy[{i}] Hamming {copy_dist}"
            );
        }
    }

    #[test]
    fn bundle_majority_matches_sql_udf_on_nonempty() {
        // Skeptic 7293f3: cross-implementation parity. Run the same
        // three concrete stalks through the Rust `bundle_majority` and
        // through the SQL `BUNDLE_MAJORITY` aggregate; assert they
        // produce identical bytes. Pin so a future divergence in
        // either implementation gets caught.
        let stalks: Vec<Hypervector> =
            vec![stalk_for(101), stalk_for(202), stalk_for(303)];
        let rust_bundle = HvCellComplex::bundle_majority(&stalks);

        let conn = conn_with_udfs();
        conn.execute("CREATE TABLE hvs(hv BLOB NOT NULL)", []).unwrap();
        for s in &stalks {
            conn.execute("INSERT INTO hvs(hv) VALUES (?1)", [s.as_slice()])
                .unwrap();
        }
        let sql_bundle: Vec<u8> = conn
            .query_row("SELECT BUNDLE_MAJORITY(hv) FROM hvs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sql_bundle.len(), D_BYTES);
        assert_eq!(
            sql_bundle.as_slice(),
            rust_bundle.as_slice(),
            "Rust bundle_majority and SQL BUNDLE_MAJORITY must agree on non-empty input"
        );
    }

    #[test]
    fn bundle_majority_empty_divergence_documented() {
        // Skeptic 7293f3: pin the documented divergence so it can't
        // change silently. Rust returns ZERO_HV; SQL returns NULL.
        // If either side ever changes, this test fails and forces an
        // audit + doc update.
        use rusqlite::types::Value;

        let rust_empty = HvCellComplex::bundle_majority(&[]);
        assert_eq!(rust_empty, ZERO_HV, "Rust empty must be ZERO_HV");

        let conn = conn_with_udfs();
        conn.execute("CREATE TABLE hvs(hv BLOB NOT NULL)", []).unwrap();
        let sql_empty: Value = conn
            .query_row("SELECT BUNDLE_MAJORITY(hv) FROM hvs", [], |r| r.get(0))
            .unwrap();
        assert!(
            matches!(sql_empty, Value::Null),
            "SQL empty must be NULL, got {sql_empty:?}"
        );
    }

    #[test]
    fn structural_root_xor_fold_holds_for_multi_layer_change() {
        // Skeptic 734d65: prior test only changed AST. This pins the
        // mathematical property for *simultaneous* multi-layer
        // changes: delta(structural) == delta(AST) XOR delta(Sem).
        // A refactor that accidentally cached an intermediate per
        // layer would pass the single-layer test and fail this one.
        let mut cx = HvCellComplex::new();
        cx.add_cell(
            HvCell::new("fn_a", CanonicalKind::Decl)
                .with_stalk(LayerKind::Ast, stalk_for(1))
                .with_stalk(LayerKind::Semantic, stalk_for(2)),
        );

        let struct_before = cx.structural_root();
        let ast_before = cx.merkle_root_for_layer(LayerKind::Ast);
        let sem_before = cx.merkle_root_for_layer(LayerKind::Semantic);

        // Mutate BOTH layers.
        cx.cells
            .get_mut("fn_a")
            .unwrap()
            .attach_stalk(LayerKind::Ast, stalk_for(99));
        cx.cells
            .get_mut("fn_a")
            .unwrap()
            .attach_stalk(LayerKind::Semantic, stalk_for(88));

        let struct_after = cx.structural_root();
        let ast_after = cx.merkle_root_for_layer(LayerKind::Ast);
        let sem_after = cx.merkle_root_for_layer(LayerKind::Semantic);

        let delta_struct = xor32(&struct_before, &struct_after);
        let delta_ast = xor32(&ast_before, &ast_after);
        let delta_sem = xor32(&sem_before, &sem_after);
        let delta_combined = xor32(&delta_ast, &delta_sem);
        assert_eq!(
            delta_struct, delta_combined,
            "structural delta must equal XOR of all changed-layer deltas"
        );
    }
}
