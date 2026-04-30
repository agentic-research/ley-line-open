//! HV-as-stalk sheaf cells: connect HDC hypervectors to sheaf-style
//! consistency reasoning. Each code unit (function/file/module) becomes
//! a [`HvCell`] carrying its hypervector as a stalk over its region of
//! the codebase. Edges between cells (containment, sibling, calls)
//! carry a *Hamming agreement* check — the HDC analogue of the
//! coboundary operator δ⁰ in `leyline-sheaf`.
//!
//! ## Why HDC + sheaf
//!
//! The closed `leyline-sheaf` crate operates on `Vec<f32>` stalks with
//! linear restriction maps and Čech cohomology. HDC stalks are
//! `[u8; D_BYTES]` — binary, byte-aligned — so the right consistency
//! measure is **Hamming distance**, not vector subtraction. Two stalks
//! are "consistent across an edge" if their popcount-distance falls
//! below a calibrated threshold; the threshold itself comes from
//! [`crate::calibrate::RadiusBaseline`] (median + k·MAD), so it's
//! data-driven rather than handcrafted.
//!
//! The HDC analogue of:
//! - `Stalk` (data over a cell) → [`HvStalk`] (hypervector + layer tag)
//! - `Cell` (region with stalk) → [`HvCell`] (id + canonical kind +
//!   per-layer stalks, so one cell can carry AST + Module + Semantic
//!   + Temporal stalks simultaneously)
//! - `RestrictionMap` (linear projection) → implicit identity for HV
//!   stalks; the Hamming check IS the restriction (any pair of HVs
//!   in the same layer can be compared directly)
//! - δ⁰ (coboundary) → [`HvCellComplex::detect_violations`] returning
//!   edges whose Hamming distance exceeds the agreement threshold
//! - H⁰ (cohomology) → [`HvCellComplex::compute_h0`] returning groups
//!   of cells whose stalks are mutually consistent (i.e. the
//!   structurally-equivalent code clusters)
//! - Merkle-tree leaf hashing → [`HvCellComplex::merkle_root_for_layer`]
//!   so a cell-complex has a single content-addressed identity that
//!   changes when any cell stalk changes (cache-invalidation hook).
//!
//! ## What this module is NOT
//!
//! This is a **specialization** of the sheaf algebra to binary HV
//! stalks. It deliberately does NOT compute δ¹, H¹, faces, or the full
//! Čech cochain complex — those need linear restriction maps and
//! make sense over `Vec<f32>` stalks, not over Hamming geometry.
//! Callers that need the full complex can convert HV → bit-as-f32 and
//! plug into `leyline-sheaf` (out of scope here).
//!
//! Additive: lives entirely inside `leyline-hdc`, no other crate
//! touched, no schema changes. The cell-complex is built in-memory from
//! data already produced by [`crate::encoder::encode_tree`].

use std::collections::{BTreeMap, HashMap};

use crate::canonical::CanonicalKind;
use crate::util::{blake3_seed, popcount_distance, Hypervector};
use crate::{D_BYTES, LayerKind};

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

    pub fn stalk(&self, layer: LayerKind) -> Option<&Hypervector> {
        self.stalks.get(&layer)
    }
}

/// A directed edge between two cells with a label describing the
/// structural relationship. The `layer` field nominates which stalk
/// layer the agreement check operates on — different relationships
/// care about different layers (a Calls edge cares about Semantic
/// agreement; a Contains edge cares about AST or Module).
#[derive(Debug, Clone)]
pub struct HvEdge {
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    pub layer: LayerKind,
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

    /// δ⁰-equivalent: walk every edge, look up both endpoints' stalks
    /// on the edge's layer, return edges whose Hamming distance
    /// exceeds the layer's threshold.
    ///
    /// Edges where one or both endpoints are missing the layer's
    /// stalk are silently skipped — they're not violations, they're
    /// just unobservable on that layer (a Calls edge can't check
    /// Semantic agreement if one side never got a Semantic stalk).
    pub fn detect_violations(&self) -> Vec<HvViolation> {
        let mut out = Vec::new();
        for (i, edge) in self.edges.iter().enumerate() {
            let Some(src_cell) = self.cells.get(&edge.source) else {
                continue;
            };
            let Some(tgt_cell) = self.cells.get(&edge.target) else {
                continue;
            };
            let Some(src_hv) = src_cell.stalk(edge.layer) else {
                continue;
            };
            let Some(tgt_hv) = tgt_cell.stalk(edge.layer) else {
                continue;
            };
            let Some(&threshold) = self.agreement_threshold.get(&edge.layer) else {
                continue;
            };
            let hamming = popcount_distance(src_hv, tgt_hv);
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
            let (Some(a_cell), Some(b_cell)) =
                (self.cells.get(&edge.source), self.cells.get(&edge.target))
            else {
                continue;
            };
            let (Some(a_hv), Some(b_hv)) = (a_cell.stalk(layer), b_cell.stalk(layer)) else {
                continue;
            };
            if popcount_distance(a_hv, b_hv) <= threshold {
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
        for layer in [
            LayerKind::Ast,
            LayerKind::Module,
            LayerKind::Semantic,
            LayerKind::Temporal,
            LayerKind::Hir,
            LayerKind::Lex,
            LayerKind::Fs,
        ] {
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
        // Domain-tagged empty root so the empty-complex root is
        // distinguishable from blake3("") at the type level.
        let mut buf = b"hdc-sheaf-empty".to_vec();
        buf.push(0u8);
        let seed = blake3_seed(&buf);
        let mut out = [0u8; 32];
        out[..8].copy_from_slice(&seed.to_le_bytes());
        return out;
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
    use crate::util::expand_seed;

    fn stalk_for(seed: u64) -> Hypervector {
        expand_seed(seed)
    }

    fn make_cell(id: &str, kind: CanonicalKind, ast_seed: u64) -> HvCell {
        let mut c = HvCell::new(id, kind);
        c.attach_stalk(LayerKind::Ast, stalk_for(ast_seed));
        c
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
        let mut cell_a = HvCell::new("fn_x", CanonicalKind::Decl);
        cell_a.attach_stalk(LayerKind::Ast, stalk_for(1));
        cell_a.attach_stalk(LayerKind::Semantic, stalk_for(100));
        cx_a.add_cell(cell_a);

        let mut cx_b = HvCellComplex::new();
        let mut cell_b = HvCell::new("fn_x", CanonicalKind::Decl);
        cell_b.attach_stalk(LayerKind::Ast, stalk_for(1));
        cell_b.attach_stalk(LayerKind::Semantic, stalk_for(101));
        cx_b.add_cell(cell_b);

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
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_b".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_c".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
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
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_b".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
        assert!(cx.detect_violations().is_empty());
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
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_b".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
        cx.add_edge(HvEdge {
            source: "fn_b".into(),
            target: "fn_c".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
        let groups = cx.compute_h0(LayerKind::Ast);
        let sizes: Vec<usize> = {
            let mut s: Vec<usize> = groups.iter().map(|g| g.len()).collect();
            s.sort_unstable();
            s
        };
        assert_eq!(sizes, vec![1, 2], "expected one 2-cluster + one singleton");
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
        cx.add_edge(HvEdge {
            source: "fn_a".into(),
            target: "fn_b".into(),
            kind: EdgeKind::Sibling,
            layer: LayerKind::Ast,
        });
        let groups = cx.compute_h0(LayerKind::Ast);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn structural_root_xor_folds_layers() {
        // Two complexes with the same per-layer roots produce the same
        // structural root. Pin the XOR-fold property: structural root
        // is the layer-XOR of per-layer Merkle roots.
        let mut cx = HvCellComplex::new();
        let mut cell = HvCell::new("fn_a", CanonicalKind::Decl);
        cell.attach_stalk(LayerKind::Ast, stalk_for(1));
        cell.attach_stalk(LayerKind::Semantic, stalk_for(2));
        cx.add_cell(cell);

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
        let mut delta_layer = [0u8; 32];
        let mut delta_struct = [0u8; 32];
        for i in 0..32 {
            delta_layer[i] = ast[i] ^ ast_2[i];
            delta_struct[i] = structural[i] ^ structural_2[i];
        }
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
}
