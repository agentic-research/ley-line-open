//! Domain-independent Čech cochain complex.
//!
//! Stalks are arbitrary-dimension `Vec<f32>`, restriction maps are
//! `DMatrix<f32>` projections, and edge taxonomy is a generic label.
//!
//! The complex computes:
//! - δ⁰: C⁰ → C¹ (coboundary operator — checks stalk consistency across edges)
//! - δ¹: C¹ → C² (cycle detection — checks face consistency)
//! - H⁰ = ker(δ⁰) (globally consistent stalks — valid cache entries)
//! - H¹ = ker(δ¹) / im(δ⁰) (independent cycles — fundamental invalidation paths)
//!
//! ## Shared `cells` keyspace invariant
//!
//! 0-cells (nodes) and 1-cells (edges) live in ONE `cells: HashMap<u32,
//! Cell>` — an id used for a node can never be reused for an edge or vice
//! versa, or the later insert silently overwrites the earlier cell and
//! corrupts the incidence structure. [`Self::add_node`] and
//! [`Self::add_edge`] assert this (bead `ley-line-open-4fece1`).
//!
//! Producers keep the spaces apart by convention: node/region ids are
//! assigned from 0 upward and edge ids from `EDGE_ID_BASE = 1_000_000`
//! upward (see [`Self::apply_delta`], the daemon's `sheaf_ops`, and
//! `ComplexBuildPass`). The convention caps the node universe at 1M ids —
//! every producer must ASSERT or guard that bound rather than assume it,
//! because the collision is silent without the cell-level asserts here.

use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(any(test, feature = "test-spy"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use nalgebra::{DMatrix, DVector};
use nalgebra_sparse::{CooMatrix, CscMatrix};

use crate::sparse::SparseOps;

// ---------------------------------------------------------------------------
// Mechanical-reach spy for L10's Gate 3 (ADR-0020 §3 / bead ley-line-open-c8090f).
//
// ADR-0020 falsifiability Gate 3 requires that the `agreement` daemon op
// MECHANICALLY reach `detect_violations` — not just "this typechecks".
// The L10 Gate 3 fixture test reads this counter to prove the call path
// actually went through the sheaf algebra rather than a degenerate
// short-circuit.
//
// Gated under `cfg(any(test, feature = "test-spy"))` so the atomic
// add is not paid in release builds without the feature. The same
// shape can be reused by L9's `ComplexBuildPass` Gate 2 spy (separate
// counter would be added there).
// ---------------------------------------------------------------------------

/// Test-only counter incremented on every entry into
/// [`CellComplex::detect_violations`]. The integration test for the L10
/// `agreement` op reads this to assert mechanical reach (the load-bearing
/// half of ADR-0020 Gate 3: "verify the path went through `detect_violations`").
#[cfg(any(test, feature = "test-spy"))]
pub static DETECT_VIOLATIONS_REACH_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Numerical tolerance for zero comparisons.
const EPS: f32 = 1e-4;

/// Sparse matrix entry threshold — values below this are treated as zero.
const SPARSE_EPS: f32 = 1e-10;

/// Maximum dense matrix elements before falling back to skip exact SVD.
const MAX_DENSE_ELEMENTS: usize = 10_000_000;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A stalk: the data assigned to a cell. Dimension is dynamic.
#[derive(Debug, Clone)]
pub struct Stalk {
    pub data: Vec<f32>,
}

impl Stalk {
    pub fn new(data: Vec<f32>) -> Self {
        Self { data }
    }

    pub fn dim(&self) -> usize {
        self.data.len()
    }
}

/// A restriction map: projects a node's stalk onto an edge's agreement space.
///
/// For an edge connecting nodes u and v with stalk dimension S and edge
/// agreement dimension D, the restriction map is a D×S matrix that extracts
/// the relevant coordinates.
#[derive(Debug, Clone)]
pub struct RestrictionMap {
    pub matrix: DMatrix<f32>,
}

impl RestrictionMap {
    /// Create a restriction map from a D×S matrix.
    pub fn new(matrix: DMatrix<f32>) -> Self {
        Self { matrix }
    }

    /// Create a 1D projection that extracts a single coordinate.
    pub fn project_dim(stalk_dim: usize, coord: usize) -> Self {
        let mut m = DMatrix::zeros(1, stalk_dim);
        m[(0, coord)] = 1.0;
        Self { matrix: m }
    }

    /// Create an identity restriction (all dimensions agree).
    pub fn identity(dim: usize) -> Self {
        Self {
            matrix: DMatrix::identity(dim, dim),
        }
    }

    /// Project the first `agreement_dim` coordinates of an `stalk_dim`-wide
    /// stalk onto a same-shaped agreement subspace. Equivalent to selecting
    /// rows of an identity matrix; cheap to build, cheap to evaluate, and
    /// the natural restriction shape for the daemon's wire-side
    /// `agreement_dim` field where the caller hasn't supplied a custom
    /// matrix.
    pub fn project_dim_range(stalk_dim: usize, agreement_dim: usize) -> Self {
        assert!(
            agreement_dim <= stalk_dim,
            "agreement_dim ({agreement_dim}) cannot exceed stalk_dim ({stalk_dim})",
        );
        let mut m = DMatrix::zeros(agreement_dim, stalk_dim);
        for i in 0..agreement_dim {
            m[(i, i)] = 1.0;
        }
        Self { matrix: m }
    }

    /// Create a weighted projection: extracts `coords` with `weights`.
    pub fn weighted(stalk_dim: usize, coords_weights: &[(usize, f32)]) -> Self {
        let mut m = DMatrix::zeros(1, stalk_dim);
        for &(coord, weight) in coords_weights {
            m[(0, coord)] = weight;
        }
        Self { matrix: m }
    }

    /// Output (edge agreement) dimension — number of rows in the projection matrix.
    pub fn nrows(&self) -> usize {
        self.matrix.nrows()
    }

    /// Input (node stalk) dimension — number of columns in the projection matrix.
    pub fn ncols(&self) -> usize {
        self.matrix.ncols()
    }

    /// Compose two restriction maps: `(outer ∘ inner)` applied to a stalk vector
    /// yields `outer.matrix * inner.matrix * x`. Used by transitive closure to
    /// build virtual edges whose restriction respects the presheaf composition
    /// axiom (rather than cloning either endpoint's map).
    ///
    /// # Panics
    /// Panics if `outer.ncols() != inner.nrows()`.
    pub fn compose(outer: &RestrictionMap, inner: &RestrictionMap) -> Self {
        assert_eq!(
            outer.ncols(),
            inner.nrows(),
            "RestrictionMap::compose dimension mismatch: outer ncols ({}) != inner nrows ({})",
            outer.ncols(),
            inner.nrows()
        );
        Self {
            matrix: &outer.matrix * &inner.matrix,
        }
    }
}

/// A 0-cell or 1-cell in the complex.
#[derive(Debug, Clone)]
pub struct Cell {
    pub id: u32,
    /// 0 = node, 1 = edge.
    pub dimension: u8,
    pub stalk: Stalk,
    /// Optional label for edge taxonomy (domain-specific, opaque to the algebra).
    pub label: Option<String>,
    /// Whether this cell was generated by presheaf composition (transitive closure).
    pub is_virtual: bool,
}

/// A 2-cell (face) bounding a cycle of edges.
#[derive(Debug, Clone)]
pub struct Cell2 {
    pub id: u32,
    /// Edge IDs and their orientations (+1.0 or -1.0) around the face.
    pub edges: Vec<(u32, f32)>,
    /// Stalk dimension of this face.
    pub dimension: usize,
}

/// A constraint violation detected by the coboundary operator.
#[derive(Debug, Clone)]
pub struct Violation {
    pub edge_id: u32,
    pub dimension_index: usize,
    /// Negative margin = violation. Magnitude indicates severity.
    pub margin: f32,
    pub is_virtual: bool,
}

/// One node-side of a topology delta: either add a new region with a stalk,
/// remove an existing region, or overwrite an existing region's stalk.
///
/// IDs are `RegionId` (u32) so this struct stays domain-agnostic; the
/// daemon's UDS wire maps `SheafStalk.id` straight through.
#[derive(Debug, Clone, Default)]
pub struct RegionDelta {
    pub added: Vec<(u32, Vec<f32>)>,
    pub removed: Vec<u32>,
    pub updated_stalks: Vec<(u32, Vec<f32>)>,
}

/// One edge-side of a topology delta. Each entry on `added` is the same
/// shape `add_edge` already takes; `removed` is by `(source, target)` pair
/// — the same shape `incidence` stores so lookup is O(edges) per remove
/// without an extra index. Lookups are direction-insensitive: removing
/// `(a, b)` removes any edge between a and b in either direction.
#[derive(Debug, Clone, Default)]
pub struct EdgeDelta {
    pub added: Vec<EdgeSpec>,
    pub removed: Vec<(u32, u32)>,
}

/// Add-edge payload for [`CellComplex::apply_delta`]. Matches the shape
/// `add_edge` consumes today, packed into one struct so the delta can
/// move many edges at once without a giant tuple.
#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub source: u32,
    pub target: u32,
    pub agreement_dim: usize,
    pub label: Option<String>,
    pub map_source: RestrictionMap,
    pub map_target: RestrictionMap,
}

/// A complete topology delta: every node-side and edge-side change applied
/// atomically by [`CellComplex::apply_delta`]. Order of application
/// (remove-edges → remove-regions → add-regions → add-edges → update-
/// stalks) is the contract — see `apply_delta` for why.
#[derive(Debug, Clone, Default)]
pub struct TopologyDelta {
    pub regions: RegionDelta,
    pub edges: EdgeDelta,
}

impl TopologyDelta {
    /// Empty delta — applying it is a no-op and `apply_delta` returns an
    /// empty touched-set. Useful as a starting point the caller mutates
    /// in place over multiple add/remove iterations.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Output of [`CellComplex::consistency_analysis`].
///
/// `defect` is the genuine sheaf-derived `‖δ⁰(stalks)‖²` — this is the H⁰
/// distance metric and is the load-bearing "stalks disagree by this much"
/// quantity. Zero ⇔ the current section lives in ker(δ⁰), i.e. in H⁰.
///
/// `consistent_groups` is a section-dependent partition produced by
/// union-find over edges whose squared δ⁰ contribution falls below
/// `threshold`. It is **not** the H⁰ cohomology group (which is a vector
/// space; see [`CellComplex::h0_dimension`] for the canonical "dim H⁰"
/// computation via SVD nullity). Use it as a fast "which nodes currently
/// agree under the active section" heuristic, not as a sheaf invariant.
#[derive(Debug)]
pub struct H0Result {
    /// Connected-component partition of nodes under the low-defect edge
    /// subgraph for the current section. Section-dependent; not H⁰.
    pub consistent_groups: Vec<Vec<u32>>,
    /// Total defect: ‖δ⁰(stalks)‖². The real sheaf invariant — this *is* the
    /// "distance from H⁰" measurement.
    pub defect: f32,
}

// ---------------------------------------------------------------------------
// CellComplex
// ---------------------------------------------------------------------------

/// A domain-independent Čech cochain complex.
///
/// Manages 0-cells (nodes with stalks), 1-cells (edges with restriction maps),
/// and 2-cells (faces bounding cycles). Computes coboundary operators δ⁰ and δ¹,
/// detects violations, and calculates cohomology groups H⁰ and H¹.
#[derive(Clone)]
pub struct CellComplex {
    pub nodes: Vec<u32>,
    pub edges: Vec<u32>,
    pub faces: Vec<u32>,
    pub cells: HashMap<u32, Cell>,
    pub face_cells: HashMap<u32, Cell2>,
    /// (node_id, edge_id) → restriction map.
    pub restriction_maps: HashMap<(u32, u32), RestrictionMap>,
    /// edge_id → (source_node_id, target_node_id).
    pub incidence: HashMap<u32, (u32, u32)>,
    /// Stalk dimension of 0-cells. All nodes must share the same dimension.
    node_stalk_dim: usize,
}

impl CellComplex {
    /// Create an empty complex with the given node stalk dimension.
    pub fn new(node_stalk_dim: usize) -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            faces: Vec::new(),
            cells: HashMap::new(),
            face_cells: HashMap::new(),
            restriction_maps: HashMap::new(),
            incidence: HashMap::new(),
            node_stalk_dim,
        }
    }

    /// Replace an existing node's stalk data without re-adding it.
    ///
    /// Used by [`crate::cache::SheafCache`] to push f32 stalk updates into
    /// the backing complex so `detect_violations` sees the latest section.
    /// Panics if the node hasn't been added yet or the dimension is wrong —
    /// the cache must call `add_node` first.
    pub fn set_node_stalk(&mut self, id: u32, data: Vec<f32>) {
        assert_eq!(
            data.len(),
            self.node_stalk_dim,
            "Node stalk must be {}D, got {}D",
            self.node_stalk_dim,
            data.len()
        );
        let cell = self.cells.get_mut(&id).unwrap_or_else(|| {
            panic!("set_node_stalk: node {id} not in complex; call add_node first")
        });
        assert_eq!(
            cell.dimension, 0,
            "set_node_stalk: cell {id} is not a 0-cell (dimension={})",
            cell.dimension,
        );
        cell.stalk = Stalk::new(data);
    }

    /// Squared ℓ² norm of the δ⁰ output on a single edge `(source → target)`.
    ///
    /// Returns `None` if no edge connects `source` to `target` (or its
    /// restriction maps are missing). This is the per-edge defect contribution
    /// — the cache uses it as the authoritative "did this edge actually
    /// disagree?" check after the XOR-Merkle pre-filter says something
    /// changed. Equivalent to one term of `Σ‖δ⁰‖²` from
    /// [`Self::consistency_analysis`].
    ///
    /// Allocation-free hot path: walks the dense restriction matrices' raw
    /// column-major slices and accumulates the squared difference of the
    /// two restriction images one row at a time. The autovectorizer turns
    /// the inner stalk-dim loop into SIMD adds + fmas on platforms with
    /// AVX2/NEON; cross-crate LTO (release profile) lets the nalgebra
    /// matrix indexing inline into this function.
    #[inline]
    pub fn edge_violation_squared(&self, source: u32, target: u32) -> Option<f32> {
        let edge_id = self
            .incidence
            .iter()
            .find(|(_, (u, v))| *u == source && *v == target)
            .map(|(&eid, _)| eid)?;
        let f_src = self.restriction_maps.get(&(source, edge_id))?;
        let f_tgt = self.restriction_maps.get(&(target, edge_id))?;
        let x_src = &self.cells.get(&source)?.stalk.data;
        let x_tgt = &self.cells.get(&target)?.stalk.data;
        if x_src.len() != self.node_stalk_dim || x_tgt.len() != self.node_stalk_dim {
            return None;
        }

        let agreement_dim = f_src.matrix.nrows();
        if f_tgt.matrix.nrows() != agreement_dim {
            return None;
        }
        let n = self.node_stalk_dim;
        // nalgebra DMatrix is column-major: column `j` lives in a
        // contiguous slice of length `nrows`. Walking columns instead of
        // rows lets us stream stalk values once and feed each into the
        // running per-row accumulator.
        let src_slice = f_src.matrix.as_slice();
        let tgt_slice = f_tgt.matrix.as_slice();
        debug_assert_eq!(src_slice.len(), agreement_dim * n);
        debug_assert_eq!(tgt_slice.len(), agreement_dim * n);

        // image[i] = Σ_j f_tgt[i,j]·x_tgt[j] − f_src[i,j]·x_src[j]
        let mut image = vec![0.0_f32; agreement_dim];
        for j in 0..n {
            let xs = x_src[j];
            let xt = x_tgt[j];
            let col_start = j * agreement_dim;
            // Inner loop is the SIMD target: contiguous f32 reads,
            // single-precision fused multiply-add into `image`.
            for i in 0..agreement_dim {
                image[i] += tgt_slice[col_start + i] * xt - src_slice[col_start + i] * xs;
            }
        }
        Some(image.iter().map(|&v| v * v).sum())
    }

    /// Add a 0-cell (node) with the given stalk data.
    ///
    /// # Panics
    /// Panics if `data.len() != self.node_stalk_dim`, or if `id` is already
    /// occupied by a 1-cell — nodes and edges share the `cells` keyspace,
    /// and a collision would silently overwrite the edge and corrupt the
    /// incidence structure (see the module doc's keyspace invariant; bead
    /// `ley-line-open-4fece1`).
    pub fn add_node(&mut self, id: u32, data: Vec<f32>) {
        assert_eq!(
            data.len(),
            self.node_stalk_dim,
            "Node stalk must be {}D, got {}D",
            self.node_stalk_dim,
            data.len()
        );
        if let Some(existing) = self.cells.get(&id) {
            assert_eq!(
                existing.dimension, 0,
                "add_node: id {id} is already a {}-cell — node/edge id-space \
                 partition violated (shared `cells` keyspace, bead \
                 ley-line-open-4fece1)",
                existing.dimension,
            );
        }
        self.cells.insert(
            id,
            Cell {
                id,
                dimension: 0,
                stalk: Stalk::new(data),
                label: None,
                is_virtual: false,
            },
        );
        self.nodes.push(id);
    }

    /// Add a 1-cell (edge) connecting source → target with the given
    /// agreement dimension, label, and restriction maps.
    ///
    /// # Panics
    /// Panics if the restriction maps' shapes are inconsistent with the
    /// complex's `node_stalk_dim` or the requested `agreement_dim` — both
    /// `map_source` and `map_target` must be `agreement_dim × node_stalk_dim`.
    /// Without this check, `build_delta_0` would read out-of-bounds entries
    /// from the restriction matrices and silently corrupt δ⁰.
    ///
    /// Also panics if `edge_id` is already occupied by a 0-cell — nodes and
    /// edges share the `cells` keyspace, and a collision would silently
    /// overwrite the node and corrupt the incidence structure (see the
    /// module doc's keyspace invariant; bead `ley-line-open-4fece1`).
    #[allow(clippy::too_many_arguments)]
    pub fn add_edge(
        &mut self,
        edge_id: u32,
        source: u32,
        target: u32,
        agreement_dim: usize,
        label: Option<String>,
        map_source: RestrictionMap,
        map_target: RestrictionMap,
        is_virtual: bool,
    ) {
        assert_eq!(
            map_source.ncols(),
            self.node_stalk_dim,
            "map_source.ncols ({}) must equal node_stalk_dim ({}) for edge {edge_id}",
            map_source.ncols(),
            self.node_stalk_dim,
        );
        assert_eq!(
            map_target.ncols(),
            self.node_stalk_dim,
            "map_target.ncols ({}) must equal node_stalk_dim ({}) for edge {edge_id}",
            map_target.ncols(),
            self.node_stalk_dim,
        );
        assert_eq!(
            map_source.nrows(),
            agreement_dim,
            "map_source.nrows ({}) must equal agreement_dim ({}) for edge {edge_id}",
            map_source.nrows(),
            agreement_dim,
        );
        assert_eq!(
            map_target.nrows(),
            agreement_dim,
            "map_target.nrows ({}) must equal agreement_dim ({}) for edge {edge_id}",
            map_target.nrows(),
            agreement_dim,
        );
        if let Some(existing) = self.cells.get(&edge_id) {
            assert_eq!(
                existing.dimension, 1,
                "add_edge: id {edge_id} is already a {}-cell — node/edge \
                 id-space partition violated (shared `cells` keyspace, bead \
                 ley-line-open-4fece1)",
                existing.dimension,
            );
        }
        self.cells.insert(
            edge_id,
            Cell {
                id: edge_id,
                dimension: 1,
                stalk: Stalk::new(vec![0.0; agreement_dim]),
                label,
                is_virtual,
            },
        );
        self.edges.push(edge_id);
        self.incidence.insert(edge_id, (source, target));
        self.restriction_maps.insert((source, edge_id), map_source);
        self.restriction_maps.insert((target, edge_id), map_target);
    }

    /// Remove a 0-cell (node) and every edge incident to it. Idempotent: if
    /// the node isn't present this is a no-op. Returns the set of edge IDs
    /// that were removed alongside the node so callers can keep their own
    /// edge-derived state in sync (the cache uses this to drop matching
    /// δ⁰-baseline entries).
    ///
    /// Edges are dropped via [`Self::remove_edge_by_id`] so restriction maps
    /// and incidence go with them — no orphan restriction map can survive
    /// a region removal.
    pub fn remove_node(&mut self, id: u32) -> Vec<u32> {
        if !self.cells.contains_key(&id) {
            return Vec::new();
        }
        // Collect touching edges first so we don't invalidate iteration
        // when remove_edge_by_id mutates self.edges / self.incidence.
        let touching: Vec<u32> = self
            .incidence
            .iter()
            .filter(|&(_, &(u, v))| u == id || v == id)
            .map(|(&eid, _)| eid)
            .collect();
        for eid in &touching {
            self.remove_edge_by_id(*eid);
        }
        self.cells.remove(&id);
        self.nodes.retain(|&n| n != id);
        touching
    }

    /// Remove the edge connecting `source ↔ target` regardless of stored
    /// direction. Idempotent — returns `None` if no such edge exists,
    /// otherwise the removed edge's id. Direction-insensitive so callers
    /// over UDS don't have to track which way the daemon stored the edge.
    pub fn remove_edge(&mut self, source: u32, target: u32) -> Option<u32> {
        let edge_id = self
            .incidence
            .iter()
            .find(|&(_, &(u, v))| (u == source && v == target) || (u == target && v == source))
            .map(|(&eid, _)| eid)?;
        self.remove_edge_by_id(edge_id);
        Some(edge_id)
    }

    /// Remove an edge by its internal id and drop its restriction maps +
    /// incidence + cell. Faces referencing the edge are dropped too so the
    /// cochain-complex axiom `δ¹∘δ⁰ = 0` keeps holding without garbage face
    /// rows pointing at a missing edge.
    fn remove_edge_by_id(&mut self, edge_id: u32) {
        let endpoints = self.incidence.remove(&edge_id);
        if let Some((u, v)) = endpoints {
            self.restriction_maps.remove(&(u, edge_id));
            self.restriction_maps.remove(&(v, edge_id));
        }
        self.cells.remove(&edge_id);
        self.edges.retain(|&e| e != edge_id);
        // Drop faces that reference the removed edge.
        let dead_faces: Vec<u32> = self
            .face_cells
            .iter()
            .filter(|(_, face)| face.edges.iter().any(|&(eid, _)| eid == edge_id))
            .map(|(&fid, _)| fid)
            .collect();
        for fid in dead_faces {
            self.face_cells.remove(&fid);
            self.faces.retain(|&f| f != fid);
        }
    }

    /// Apply a [`TopologyDelta`] atomically and return the set of region IDs
    /// the delta touched directly (added, removed, edge-touched, or stalk-
    /// updated). The caller computes radius-1 neighbours separately — this
    /// method intentionally stays "what changed, nothing more" so the cache
    /// can layer its own neighbour expansion on top.
    ///
    /// Order matters: edges are removed before regions so removing a region
    /// doesn't try to drop the same edge twice; regions are added before
    /// edges so the new edge's endpoints exist when `add_edge` validates
    /// restriction-map shapes; stalks are updated last so the f32 data is
    /// applied against the new region set.
    ///
    /// Internal edge IDs are assigned sequentially starting from
    /// `max(edges) + 1` (or the `1M` collision-avoidance floor) so they
    /// don't collide with anything the seed `set_topology` installed.
    pub fn apply_delta(&mut self, delta: &TopologyDelta) -> Vec<u32> {
        let mut touched: HashSet<u32> = HashSet::new();

        for &(src, tgt) in &delta.edges.removed {
            if self.remove_edge(src, tgt).is_some() {
                touched.insert(src);
                touched.insert(tgt);
            }
        }

        for &rid in &delta.regions.removed {
            let dropped_edges = self.remove_node(rid);
            // Mark the dropped-edge endpoints as touched too — a downstream
            // baseline refresh needs to see them.
            for eid in dropped_edges {
                if let Some(&(u, v)) = self.incidence.get(&eid) {
                    touched.insert(u);
                    touched.insert(v);
                }
            }
            touched.insert(rid);
        }

        for (rid, data) in &delta.regions.added {
            // Skip re-adds of an already-present region; the stalk-update
            // pass below handles overwrites cleanly.
            if !self.cells.contains_key(rid) {
                self.add_node(*rid, data.clone());
            } else {
                self.set_node_stalk(*rid, data.clone());
            }
            touched.insert(*rid);
        }

        // Edge IDs share the `cells` namespace with regions. Start from a
        // high offset (1_000_000) to avoid collision with region IDs the
        // daemon hands us — the seed `set_topology` uses `100` and is bounded
        // by the OSS region count, but incremental updates can run forever
        // and we don't want to depend on "region IDs stay under 100". Bump
        // forward from the current max edge ID so re-applies don't reuse
        // a still-live ID.
        const EDGE_ID_BASE: u32 = 1_000_000;
        let next_edge_id = self
            .edges
            .iter()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(EDGE_ID_BASE)
            .max(EDGE_ID_BASE);
        // Zip with the open-ended counter range instead of a manual `+= 1`
        // — clippy's `explicit_counter_loop` lint flags the latter, and
        // the zip form makes the parallel iteration explicit.
        for (edge_id, spec) in (next_edge_id..).zip(&delta.edges.added) {
            self.add_edge(
                edge_id,
                spec.source,
                spec.target,
                spec.agreement_dim,
                spec.label.clone(),
                spec.map_source.clone(),
                spec.map_target.clone(),
                false,
            );
            touched.insert(spec.source);
            touched.insert(spec.target);
        }

        for (rid, data) in &delta.regions.updated_stalks {
            if self.cells.contains_key(rid) {
                self.set_node_stalk(*rid, data.clone());
                touched.insert(*rid);
            }
        }

        let mut out: Vec<u32> = touched.into_iter().collect();
        out.sort();
        out
    }

    /// Add a 2-cell (face) bounding a cycle of edges.
    ///
    /// `edges` is a list of `(edge_id, sign)` pairs. The `sign` records
    /// whether the face traverses the edge in its natural source→target
    /// direction (`+1.0`) or in reverse (`-1.0`); see
    /// [`Self::generate_faces_from_cycles`].
    ///
    /// # Panics
    /// Panics if the edge list is empty, has duplicate edges, references
    /// an unknown edge id, or does not close a cycle when oriented per
    /// the supplied signs. Without these checks the cochain-complex axiom
    /// `δ¹∘δ⁰ = 0` is unprovable for the face: `build_delta_1` would happily
    /// register garbage signs against edges that don't bound the face, and
    /// `assert_cochain_complex` would silently pass on a non-cycle face.
    pub fn add_face(&mut self, face_id: u32, edges: &[(u32, f32)], dim: usize) {
        assert!(!edges.is_empty(), "add_face: face {face_id} has no edges",);
        let mut seen = HashSet::new();
        for &(eid, sign) in edges {
            assert!(
                seen.insert(eid),
                "add_face: face {face_id} references edge {eid} more than once",
            );
            assert!(
                self.incidence.contains_key(&eid),
                "add_face: face {face_id} references unknown edge {eid}",
            );
            assert!(
                sign == 1.0 || sign == -1.0,
                "add_face: face {face_id} edge {eid} sign must be ±1.0, got {sign}",
            );
        }
        // Verify the edges form a closed cycle when oriented per their signs.
        // Walking the path: starting at the first edge's oriented source, each
        // step's oriented target must equal the next step's oriented source,
        // and the final target must close back on the initial source.
        let oriented = |eid: u32, sign: f32| -> (u32, u32) {
            let &(s, t) = self.incidence.get(&eid).expect("incidence checked above");
            if sign > 0.0 { (s, t) } else { (t, s) }
        };
        let (start, _) = oriented(edges[0].0, edges[0].1);
        let mut cursor = start;
        for &(eid, sign) in edges {
            let (s, t) = oriented(eid, sign);
            assert_eq!(
                cursor, s,
                "add_face: face {face_id} edges do not form a cycle — expected step starting at {cursor}, got edge {eid} starting at {s}",
            );
            cursor = t;
        }
        assert_eq!(
            cursor, start,
            "add_face: face {face_id} edges do not form a cycle — final cursor is {cursor}, expected to close back to start {start}",
        );

        self.face_cells.insert(
            face_id,
            Cell2 {
                id: face_id,
                edges: edges.to_vec(),
                dimension: dim,
            },
        );
        self.faces.push(face_id);
    }

    // -----------------------------------------------------------------------
    // Presheaf composition (gluing axiom)
    // -----------------------------------------------------------------------

    /// Enforce transitive closure on edges with the given label.
    ///
    /// If `A →(label) B →(label) C` exists, adds a virtual edge `A →(label) C`
    /// whose restriction maps are the **composition** of the intermediate
    /// edges' maps. Realizes the presheaf composition axiom
    /// `f_{A→C} = f_{B→C} ∘ f_{A→B}` for arbitrary (non-identity) restrictions.
    ///
    /// Paths whose composed maps don't fit the requested `agreement_dim` are
    /// skipped (no virtual edge created) — there is no silent default
    /// fallback. The caller is responsible for picking a label whose edges
    /// share a consistent agreement dimension.
    pub fn enforce_transitive_closure(&mut self, label: &str, agreement_dim: usize) {
        let mut pairs = Vec::new();
        for &edge_id in &self.edges {
            if let Some(cell) = self.cells.get(&edge_id)
                && cell.label.as_deref() == Some(label)
                && !cell.is_virtual
                && let Some(&(u, v)) = self.incidence.get(&edge_id)
            {
                pairs.push((u, v, edge_id));
            }
        }

        let mut parent_to_children: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for &(p, c, eid) in &pairs {
            parent_to_children.entry(p).or_default().push((c, eid));
        }

        let mut virtual_id = self.edges.iter().copied().max().unwrap_or(0) + 1;

        // For each start node, DFS the label-induced subgraph collecting the
        // edge path. For paths of length ≥ 2 we synthesize the composite
        // virtual edge whose source map comes from the first edge and target
        // map from the last edge along the label-induced subgraph.
        for &start in parent_to_children.keys() {
            let mut stack: Vec<(u32, Vec<u32>)> = vec![(start, Vec::new())];
            let mut visited = HashSet::new();

            while let Some((curr, path_edges)) = stack.pop() {
                if !visited.insert(curr) {
                    continue;
                }

                if path_edges.len() > 1
                    && let Some((src_map, tgt_map)) =
                        self.compose_path_maps(start, curr, &path_edges, agreement_dim)
                {
                    self.add_edge(
                        virtual_id,
                        start,
                        curr,
                        agreement_dim,
                        Some(label.to_string()),
                        src_map,
                        tgt_map,
                        true,
                    );
                    virtual_id += 1;
                }

                if let Some(children) = parent_to_children.get(&curr) {
                    for &(child, eid) in children {
                        let mut next_path = path_edges.clone();
                        next_path.push(eid);
                        stack.push((child, next_path));
                    }
                }
            }
        }
    }

    /// Compose the restriction maps along a label-edge path `start → … → end`.
    ///
    /// Returns `(src_map, tgt_map)` for the virtual edge `start → end` —
    /// `src_map` from the first edge's source-side restriction, `tgt_map`
    /// from the last edge's target-side restriction. Returns `None` if the
    /// path is empty, any intermediate map is missing, or the composed
    /// shape doesn't match `agreement_dim × node_stalk_dim`. No silent
    /// default — callers either get a valid composition or skip the edge.
    fn compose_path_maps(
        &self,
        start: u32,
        end: u32,
        edges: &[u32],
        agreement_dim: usize,
    ) -> Option<(RestrictionMap, RestrictionMap)> {
        let first_edge = *edges.first()?;
        let src_map = self.restriction_maps.get(&(start, first_edge))?.clone();
        let last_edge = *edges.last()?;
        let tgt_map = self.restriction_maps.get(&(end, last_edge))?.clone();

        if src_map.nrows() != agreement_dim || tgt_map.nrows() != agreement_dim {
            return None;
        }
        if src_map.ncols() != self.node_stalk_dim || tgt_map.ncols() != self.node_stalk_dim {
            return None;
        }

        Some((src_map, tgt_map))
    }

    // -----------------------------------------------------------------------
    // Cycle detection → 2-cell generation
    // -----------------------------------------------------------------------

    /// Generate 2-cells (faces) from cycles in edges with the given label.
    ///
    /// Finds triangles via neighbor-set intersection and longer cycles (4-8)
    /// via bounded DFS. Returns the number of faces created.
    ///
    /// Each face edge carries a sign that records whether the path traverses
    /// the stored edge in its natural source→target direction (`+1.0`) or
    /// in reverse (`-1.0`). The cochain-complex axiom `δ¹∘δ⁰ = 0` requires
    /// the per-edge sign to reflect the face's induced orientation, not a
    /// hardcoded `+1.0` — otherwise non-trivial cycles silently fail the
    /// axiom check on any heterogeneous restriction map.
    pub fn generate_faces_from_cycles(&mut self, label: &str, max_cycles: usize) -> usize {
        // Build (source, target) → edge_id map for the given label. We also
        // record the reverse direction so cycle traversal can pick up edges
        // whose natural orientation runs opposite the traversal — those edges
        // contribute with sign -1 in δ¹.
        let mut edge_map: HashMap<(u32, u32), u32> = HashMap::new();
        let mut dep_edges = Vec::new();
        for &edge_id in &self.edges {
            if let Some(cell) = self.cells.get(&edge_id)
                && cell.label.as_deref() == Some(label)
                && let Some(&(src, tgt)) = self.incidence.get(&edge_id)
            {
                edge_map.insert((src, tgt), edge_id);
                dep_edges.push((src, tgt));
            }
        }

        if dep_edges.is_empty() {
            return 0;
        }

        // Lookup helper: for a path step (s → t), find the underlying edge
        // and the sign relative to the traversal direction.
        let signed_lookup = |s: u32, t: u32| -> Option<(u32, f32)> {
            if let Some(&eid) = edge_map.get(&(s, t)) {
                Some((eid, 1.0))
            } else if let Some(&eid) = edge_map.get(&(t, s)) {
                Some((eid, -1.0))
            } else {
                None
            }
        };

        // Adjacency for cycle search must consider both directions — a face
        // can include an edge traversed in reverse.
        let mut adjacency: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut neighbor_set: HashMap<u32, HashSet<u32>> = HashMap::new();
        let mut all_nodes: HashSet<u32> = HashSet::new();

        for &(src, tgt) in &dep_edges {
            adjacency.entry(src).or_default().push(tgt);
            adjacency.entry(tgt).or_default().push(src);
            neighbor_set.entry(src).or_default().insert(tgt);
            neighbor_set.entry(tgt).or_default().insert(src);
            all_nodes.insert(src);
            all_nodes.insert(tgt);
        }

        let max_existing_id = self.edges.iter().copied().max().unwrap_or(0);
        let mut face_id = max_existing_id + 200_000;
        let mut faces_created = 0usize;
        let mut seen_cycles: HashSet<Vec<u32>> = HashSet::new();

        let canonicalize = |cycle: &[u32]| -> Vec<u32> {
            if cycle.is_empty() {
                return vec![];
            }
            let min_pos = cycle
                .iter()
                .enumerate()
                .min_by_key(|&(_, &v)| v)
                .map(|(i, _)| i)
                .unwrap_or(0);
            (0..cycle.len())
                .map(|i| cycle[(min_pos + i) % cycle.len()])
                .collect()
        };

        // Phase 1: triangles via neighbor-set intersection
        for &(a, b) in &dep_edges {
            if faces_created >= max_cycles {
                break;
            }
            if let Some(b_neighbors) = adjacency.get(&b) {
                for &c in b_neighbors {
                    if faces_created >= max_cycles {
                        break;
                    }
                    if let Some(c_nset) = neighbor_set.get(&c)
                        && c_nset.contains(&a)
                    {
                        let canon = canonicalize(&[a, b, c]);
                        if seen_cycles.contains(&canon) {
                            continue;
                        }
                        seen_cycles.insert(canon);

                        let mut face_edges = Vec::new();
                        for &(s, t) in &[(a, b), (b, c), (c, a)] {
                            if let Some((eid, sign)) = signed_lookup(s, t) {
                                face_edges.push((eid, sign));
                            }
                        }
                        if face_edges.len() == 3 {
                            // All dependency edges are 1D
                            self.add_face(face_id, &face_edges, 1);
                            face_id += 1;
                            faces_created += 1;
                        }
                    }
                }
            }
        }

        // Phase 2: longer cycles (4-8) via bounded DFS
        let mut all_nodes_sorted: Vec<u32> = all_nodes.into_iter().collect();
        all_nodes_sorted.sort();

        for &start in &all_nodes_sorted {
            if faces_created >= max_cycles {
                break;
            }
            let mut stack: Vec<(u32, Vec<u32>)> = vec![(start, vec![start])];
            let mut visited: HashSet<u32> = HashSet::new();
            visited.insert(start);

            while let Some((node, path)) = stack.pop() {
                if faces_created >= max_cycles {
                    break;
                }
                if path.len() > 8 {
                    continue;
                }
                if let Some(neighbors) = adjacency.get(&node) {
                    for &next in neighbors {
                        if faces_created >= max_cycles {
                            break;
                        }
                        if next == start && path.len() >= 4 {
                            let canon = canonicalize(&path);
                            if seen_cycles.contains(&canon) {
                                continue;
                            }
                            seen_cycles.insert(canon);

                            let mut face_edges = Vec::new();
                            let mut all_found = true;
                            for i in 0..path.len() {
                                let src = path[i];
                                let tgt = path[(i + 1) % path.len()];
                                if let Some((eid, sign)) = signed_lookup(src, tgt) {
                                    face_edges.push((eid, sign));
                                } else {
                                    all_found = false;
                                    break;
                                }
                            }
                            if all_found && face_edges.len() >= 4 {
                                self.add_face(face_id, &face_edges, 1);
                                face_id += 1;
                                faces_created += 1;
                            }
                        } else if next != start && !visited.contains(&next) && path.len() < 8 {
                            visited.insert(next);
                            let mut new_path = path.clone();
                            new_path.push(next);
                            stack.push((next, new_path));
                        }
                    }
                }
            }
        }

        faces_created
    }

    // -----------------------------------------------------------------------
    // Coboundary operators
    // -----------------------------------------------------------------------

    /// Build the δ⁰: C⁰ → C¹ coboundary matrix (sparse CSC).
    ///
    /// For each edge e = (u, v) with restriction maps f_u, f_v:
    ///   (δ⁰ x)_e = f_v(x_v) - f_u(x_u)
    pub fn build_delta_0(&self) -> CscMatrix<f32> {
        let c0_dim = self.nodes.len() * self.node_stalk_dim;
        let mut c1_dim = 0;
        let mut edge_row_offsets = HashMap::new();
        for &edge_id in &self.edges {
            edge_row_offsets.insert(edge_id, c1_dim);
            c1_dim += self
                .cells
                .get(&edge_id)
                .expect("cell/incidence populated by build loop")
                .stalk
                .dim();
        }

        let mut node_col_offsets = HashMap::new();
        for (i, &node_id) in self.nodes.iter().enumerate() {
            node_col_offsets.insert(node_id, i * self.node_stalk_dim);
        }

        let mut coo = CooMatrix::new(c1_dim, c0_dim);

        for &edge_id in &self.edges {
            let (u_id, v_id) = self
                .incidence
                .get(&edge_id)
                .expect("cell/incidence populated by build loop");
            let row_start = *edge_row_offsets
                .get(&edge_id)
                .expect("cell/incidence populated by build loop");
            let u_col_start = *node_col_offsets
                .get(u_id)
                .expect("cell/incidence populated by build loop");
            let v_col_start = *node_col_offsets
                .get(v_id)
                .expect("cell/incidence populated by build loop");

            let f_u = &self
                .restriction_maps
                .get(&(*u_id, edge_id))
                .expect("restriction map populated by build loop")
                .matrix;
            let f_v = &self
                .restriction_maps
                .get(&(*v_id, edge_id))
                .expect("restriction map populated by build loop")
                .matrix;

            // -f_u contribution
            for r in 0..f_u.nrows() {
                for c in 0..f_u.ncols() {
                    let val = -f_u[(r, c)];
                    if val.abs() > SPARSE_EPS {
                        coo.push(row_start + r, u_col_start + c, val);
                    }
                }
            }
            // +f_v contribution
            for r in 0..f_v.nrows() {
                for c in 0..f_v.ncols() {
                    let val = f_v[(r, c)];
                    if val.abs() > SPARSE_EPS {
                        coo.push(row_start + r, v_col_start + c, val);
                    }
                }
            }
        }

        CscMatrix::from(&coo)
    }

    /// Build the δ¹: C¹ → C² coboundary matrix (sparse CSC).
    pub fn build_delta_1(&self) -> CscMatrix<f32> {
        let mut c1_dim = 0;
        let mut edge_row_offsets = HashMap::new();
        for &edge_id in &self.edges {
            edge_row_offsets.insert(edge_id, c1_dim);
            c1_dim += self
                .cells
                .get(&edge_id)
                .expect("cell/incidence populated by build loop")
                .stalk
                .dim();
        }

        let mut c2_dim = 0;
        let mut face_row_offsets = HashMap::new();
        for &face_id in &self.faces {
            face_row_offsets.insert(face_id, c2_dim);
            c2_dim += self
                .face_cells
                .get(&face_id)
                .expect("cell/incidence populated by build loop")
                .dimension;
        }

        let mut coo = CooMatrix::new(c2_dim, c1_dim);

        for &face_id in &self.faces {
            let face = self
                .face_cells
                .get(&face_id)
                .expect("cell/incidence populated by build loop");
            let f_row = *face_row_offsets
                .get(&face_id)
                .expect("cell/incidence populated by build loop");

            for &(edge_id, sign) in &face.edges {
                let e_col = *edge_row_offsets
                    .get(&edge_id)
                    .expect("cell/incidence populated by build loop");
                let e_dim = self
                    .cells
                    .get(&edge_id)
                    .expect("cell/incidence populated by build loop")
                    .stalk
                    .dim();

                for i in 0..face.dimension.min(e_dim) {
                    if sign.abs() > SPARSE_EPS {
                        coo.push(f_row + i, e_col + i, sign);
                    }
                }
            }
        }

        CscMatrix::from(&coo)
    }

    /// Assert the cochain complex condition: δ¹ ∘ δ⁰ = 0.
    ///
    /// # Panics
    /// Panics if the condition is violated (max entry > 1e-4).
    ///
    /// # Resource cap
    /// Above `MAX_DENSE_ELEMENTS` the dense product is not formed and the
    /// axiom is NOT verified for this complex. That skip is logged at
    /// `warn` — silent verification disappearance is worse than a size
    /// limit, because callers read "no panic" as "axiom held" (bead
    /// `ley-line-open-504341`, P7a).
    pub fn assert_cochain_complex(&self) {
        if self.faces.is_empty() || self.edges.is_empty() {
            return;
        }
        let d0 = self.build_delta_0();
        let d1 = self.build_delta_1();

        let product_elements = d1.nrows() * d0.ncols();
        if product_elements > MAX_DENSE_ELEMENTS {
            log::warn!(
                "assert_cochain_complex: SKIPPED — δ¹∘δ⁰ = 0 axiom NOT \
                 verified for this complex ({} × {} product = {} elements \
                 > MAX_DENSE_ELEMENTS = {}). Absence of a panic here is \
                 not evidence the axiom holds.",
                d1.nrows(),
                d0.ncols(),
                product_elements,
                MAX_DENSE_ELEMENTS,
            );
            return;
        }

        let dense_0 = SparseOps::to_dense(&d0);
        let dense_1 = SparseOps::to_dense(&d1);
        let product = &dense_1 * &dense_0;
        let max_entry = product.abs().max();
        assert!(
            max_entry < EPS,
            "δ¹∘δ⁰ ≠ 0 (max entry: {}). Cochain complex is invalid.",
            max_entry
        );
    }

    // -----------------------------------------------------------------------
    // Violation detection + cohomology
    // -----------------------------------------------------------------------

    /// Concatenate all node stalks into a single global section vector.
    pub fn global_section(&self) -> DVector<f32> {
        let mut x = Vec::with_capacity(self.nodes.len() * self.node_stalk_dim);
        for node_id in &self.nodes {
            x.extend_from_slice(
                &self
                    .cells
                    .get(node_id)
                    .expect("cell/incidence populated by build loop")
                    .stalk
                    .data,
            );
        }
        DVector::from_vec(x)
    }

    /// Detect constraint violations via δ⁰. O(nnz).
    ///
    /// Returns edges where the coboundary margin is negative (constraint
    /// violated). Margin magnitude indicates severity.
    pub fn detect_violations(&self) -> Vec<Violation> {
        let delta = self.build_delta_0();
        let x = self.global_section();

        assert_eq!(
            delta.ncols(),
            x.len(),
            "Dimension mismatch: δ⁰ is {}×{}, section is {}",
            delta.nrows(),
            delta.ncols(),
            x.len()
        );

        let bounds = SparseOps::spmv(&delta, &x);
        let mut violations = Vec::new();
        let mut row_offset = 0;

        for &edge_id in &self.edges {
            let cell = self
                .cells
                .get(&edge_id)
                .expect("cell/incidence populated by build loop");
            let dim = cell.stalk.dim();

            for i in 0..dim {
                let val = bounds[row_offset + i];
                // Symmetric: any non-zero δ⁰ output magnitude beyond
                // numerical-noise EPS is a violation, regardless of sign.
                // δ⁰ = f_v(x_v) − f_u(x_u), so the sign flips when the
                // endpoints' roles flip; agreement is a magnitude property.
                if val.abs() > EPS {
                    violations.push(Violation {
                        edge_id,
                        dimension_index: i,
                        margin: val,
                        is_virtual: cell.is_virtual,
                    });
                }
            }
            row_offset += dim;
        }

        // Mechanical-reach spy for ADR-0020 §3 Gate 3 (bead
        // ley-line-open-c8090f). Placed AFTER `build_delta_0` + spmv
        // + the violation-collection loop so the counter only fires
        // when the math actually executed end-to-end. A regression
        // that early-returns before the algebra still fails the L10
        // gate, per math-friend bead `ley-line-open-65d0bf`.
        #[cfg(any(test, feature = "test-spy"))]
        DETECT_VIOLATIONS_REACH_COUNT.fetch_add(1, Ordering::Relaxed);

        violations
    }

    /// Section-dependent consistency analysis: total `‖δ⁰‖²` defect plus a
    /// union-find partition of nodes into groups connected by low-defect
    /// edges (defect below `threshold`).
    ///
    /// **Not** the H⁰ cohomology group — see [`Self::h0_dimension`] for the
    /// algebraic `dim ker(δ⁰)`. The `defect` field IS a real sheaf invariant
    /// (the H⁰ distance metric); the `consistent_groups` partition is a
    /// section-dependent observable used by the cache heuristic.
    ///
    /// `compute_h0` is retained as a thin alias for backward compatibility
    /// with the lift's existing test surface; new callers should prefer
    /// [`Self::consistency_analysis`].
    pub fn consistency_analysis(&self, threshold: f32) -> H0Result {
        let delta = self.build_delta_0();
        let x = self.global_section();
        let image = SparseOps::spmv(&delta, &x);

        // Compute per-edge squared norms
        let mut edge_defects: HashMap<u32, f32> = HashMap::new();
        let mut row_offset = 0;
        for &edge_id in &self.edges {
            let dim = self
                .cells
                .get(&edge_id)
                .expect("cell/incidence populated by build loop")
                .stalk
                .dim();
            let mut norm_sq = 0.0f32;
            for i in 0..dim {
                let v = image[row_offset + i];
                norm_sq += v * v;
            }
            edge_defects.insert(edge_id, norm_sq);
            row_offset += dim;
        }

        let total_defect: f32 = edge_defects.values().sum();

        // Union-find: merge nodes connected by low-defect edges
        let mut parent: HashMap<u32, u32> = HashMap::new();
        for &n in &self.nodes {
            parent.insert(n, n);
        }

        fn find(parent: &mut HashMap<u32, u32>, x: u32) -> u32 {
            let p = *parent.get(&x).unwrap_or(&x);
            if p == x {
                return x;
            }
            let root = find(parent, p);
            parent.insert(x, root);
            root
        }

        for &edge_id in &self.edges {
            if let Some(&defect) = edge_defects.get(&edge_id)
                && defect < threshold
                && let Some(&(u, v)) = self.incidence.get(&edge_id)
            {
                let ru = find(&mut parent, u);
                let rv = find(&mut parent, v);
                if ru != rv {
                    parent.insert(ru, rv);
                }
            }
        }

        // Collect groups keyed by union-find root. BTreeMap (not HashMap)
        // so iteration order is deterministic across runs — consistent_groups
        // is consumed by the cache layer, which expects stable orderings to
        // avoid hash-seed-dependent test flakiness.
        let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &n in &self.nodes {
            let root = find(&mut parent, n);
            groups.entry(root).or_default().push(n);
        }

        H0Result {
            consistent_groups: groups.into_values().collect(),
            defect: total_defect,
        }
    }

    /// Canonical `dim H⁰` = `dim ker(δ⁰)` via SVD numerical nullity over the
    /// rows of δ⁰ active under the current section.
    ///
    /// This is the **algebraic** sheaf invariant — independent of the choice
    /// of section, depending only on the complex's restriction maps and the
    /// active edge set. Contrast with [`Self::consistency_analysis`], which
    /// reports the *current section's* defect and partition.
    ///
    /// "Active" rows are the SATISFIED constraints — `|δ⁰ᵢ| ≤ EPS`,
    /// symmetric in sign like [`Self::detect_violations`]. δ⁰ flips sign
    /// under endpoint-orientation swap, so a signed comparison would make
    /// the reported dimension orientation-dependent (bead
    /// `ley-line-open-4e8a8f`).
    ///
    /// Returns `None` when the active system exceeds `MAX_DENSE_ELEMENTS`
    /// and the exact SVD is refused. `None` means "unanswerable at this
    /// size", NOT "dim H⁰ = 0" — a `0` would claim no globally consistent
    /// section exists, which is a semantic wrong answer, not a resource
    /// condition. Callers must surface the `None`, not default it.
    pub fn h0_dimension(&self) -> Option<usize> {
        let delta = self.build_delta_0();
        let x = self.global_section();
        let bounds = SparseOps::spmv(&delta, &x);

        // Symmetric |·| ≤ EPS: satisfied rows only, regardless of the
        // sign convention induced by edge-endpoint order. Mirrors the
        // detect_violations fix (`val.abs() > EPS`).
        let active_rows: Vec<usize> = (0..bounds.len())
            .filter(|&i| bounds[i].abs() <= EPS)
            .collect();

        if active_rows.is_empty() {
            return Some(self.nodes.len() * self.node_stalk_dim);
        }

        let ncols = delta.ncols();
        let active_elements = active_rows.len() * ncols;
        if active_elements > MAX_DENSE_ELEMENTS {
            // Refuse rather than fabricate: see doc comment.
            return None;
        }

        let mut row_map: HashMap<usize, usize> = HashMap::with_capacity(active_rows.len());
        for (new_r, &old_r) in active_rows.iter().enumerate() {
            row_map.insert(old_r, new_r);
        }

        let mut delta_active = DMatrix::zeros(active_rows.len(), ncols);
        for col_idx in 0..ncols {
            let col = delta.col(col_idx);
            for (&row_idx, &val) in col.row_indices().iter().zip(col.values().iter()) {
                if let Some(&new_r) = row_map.get(&row_idx) {
                    delta_active[(new_r, col_idx)] = val;
                }
            }
        }

        let svd = delta_active.svd(true, true);
        let rank = SparseOps::numerical_rank(&svd);
        Some(ncols - rank)
    }

    /// Compute the H¹ Betti number: dim(ker(δ¹) / im(δ⁰)).
    ///
    /// Measures independent cycles not bounded by faces. Uses randomized
    /// SVD (Halko-Martinsson-Tropp) for large matrices.
    pub fn h1_betti_number(&self) -> usize {
        if self.faces.is_empty() {
            return 0;
        }

        let delta_0 = self.build_delta_0();
        let delta_1 = self.build_delta_1();

        let max_rank_hint = self.faces.len() + self.edges.len();
        let rank_1 = SparseOps::rank_estimate(&delta_1, max_rank_hint);
        let nullity_1 = delta_1.ncols().saturating_sub(rank_1);
        let rank_0 = SparseOps::rank_estimate(&delta_0, max_rank_hint);

        nullity_1.saturating_sub(rank_0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a simple 2-node complex with a 1D agreement edge.
    fn two_node_complex(a_val: f32, b_val: f32) -> CellComplex {
        let mut cx = CellComplex::new(2);
        cx.add_node(0, vec![a_val, 0.0]);
        cx.add_node(1, vec![b_val, 0.0]);
        // Edge checks that first coordinate agrees
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("test".into()),
            RestrictionMap::project_dim(2, 0),
            RestrictionMap::project_dim(2, 0),
            false,
        );
        cx
    }

    #[test]
    fn consistent_nodes_no_violations() {
        let cx = two_node_complex(5.0, 5.0);
        let violations = cx.detect_violations();
        assert!(violations.is_empty());
    }

    #[test]
    fn inconsistent_nodes_have_violations() {
        // a=5, b=3 → coboundary = f_v(b) - f_u(a) = 3 - 5 = -2 < 0
        let cx = two_node_complex(5.0, 3.0);
        let violations = cx.detect_violations();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].margin < 0.0);
    }

    #[test]
    fn inconsistent_nodes_have_violations_symmetric_positive_margin() {
        // a=3, b=5 → coboundary = f_v(b) - f_u(a) = 5 - 3 = +2 > 0.
        // Prior to the symmetric `val.abs() > EPS` fix this case slipped
        // through `detect_violations` silently — a real bug where the
        // direction of disagreement determined visibility.
        let cx = two_node_complex(3.0, 5.0);
        let violations = cx.detect_violations();
        assert_eq!(
            violations.len(),
            1,
            "positive-margin disagreement must produce a violation",
        );
        assert!(
            violations[0].margin > 0.0,
            "margin should be positive for a=3, b=5; got {}",
            violations[0].margin,
        );
    }

    #[test]
    #[should_panic(expected = "must equal node_stalk_dim")]
    fn add_edge_rejects_restriction_with_wrong_ncols() {
        let mut cx = CellComplex::new(3);
        cx.add_node(0, vec![0.0; 3]);
        cx.add_node(1, vec![0.0; 3]);
        // Restriction projects from a 2D stalk — incompatible with the
        // complex's 3D node_stalk_dim. Without the dim check this would
        // produce garbage δ⁰ output (or panic deep in build_delta_0).
        let bad = RestrictionMap::project_dim(2, 0);
        cx.add_edge(100, 0, 1, 1, None, bad.clone(), bad, false);
    }

    #[test]
    #[should_panic(expected = "must equal agreement_dim")]
    fn add_edge_rejects_restriction_with_wrong_nrows() {
        let mut cx = CellComplex::new(2);
        cx.add_node(0, vec![0.0; 2]);
        cx.add_node(1, vec![0.0; 2]);
        // Restriction outputs 1D but caller asked for agreement_dim=2.
        let bad = RestrictionMap::project_dim(2, 0);
        cx.add_edge(100, 0, 1, 2, None, bad.clone(), bad, false);
    }

    /// Bead ley-line-open-4fece1: nodes and edges share the `cells`
    /// keyspace. Before the partition assert, an edge id reused as a node
    /// id silently overwrote the edge cell — corrupted incidence with no
    /// signal. Now it panics loud.
    #[test]
    #[should_panic(expected = "id-space partition violated")]
    fn add_node_rejects_id_already_used_by_edge() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![0.0]);
        cx.add_node(1, vec![0.0]);
        let p = RestrictionMap::identity(1);
        cx.add_edge(100, 0, 1, 1, None, p.clone(), p, false);
        // Node id 100 collides with edge cell 100.
        cx.add_node(100, vec![0.0]);
    }

    /// Companion: an edge id reused from a node id panics too.
    #[test]
    #[should_panic(expected = "id-space partition violated")]
    fn add_edge_rejects_id_already_used_by_node() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![0.0]);
        cx.add_node(1, vec![0.0]);
        cx.add_node(2, vec![0.0]);
        let p = RestrictionMap::identity(1);
        // Edge id 2 collides with node cell 2.
        cx.add_edge(2, 0, 1, 1, None, p.clone(), p, false);
    }

    #[test]
    fn h0_groups_consistent_nodes() {
        let mut cx = CellComplex::new(2);
        cx.add_node(0, vec![1.0, 0.0]);
        cx.add_node(1, vec![1.0, 0.0]);
        cx.add_node(2, vec![9.0, 0.0]);
        // 0-1 agree, 1-2 disagree
        cx.add_edge(
            100,
            0,
            1,
            1,
            None,
            RestrictionMap::project_dim(2, 0),
            RestrictionMap::project_dim(2, 0),
            false,
        );
        cx.add_edge(
            101,
            1,
            2,
            1,
            None,
            RestrictionMap::project_dim(2, 0),
            RestrictionMap::project_dim(2, 0),
            false,
        );

        let h0 = cx.consistency_analysis(0.01);
        // Nodes 0 and 1 should be in the same group, node 2 separate
        assert!(h0.defect > 0.0);
        let has_pair = h0.consistent_groups.iter().any(|g| g.len() == 2);
        assert!(has_pair);
    }

    /// F2 (bead ley-line-open-4e8a8f): `dim H⁰` is an algebraic invariant
    /// of the complex — it must not depend on the ORDER in which an edge's
    /// endpoints were supplied. δ⁰ = f_v(x_v) − f_u(x_u) flips sign when
    /// the endpoints' roles flip, so a signed `bounds[i] <= EPS` row filter
    /// admits a violated row in one orientation and rejects it in the
    /// other, producing two different "dimensions" for the same complex.
    #[test]
    fn h0_dimension_is_orientation_invariant() {
        let build = |src: u32, tgt: u32| {
            let mut cx = CellComplex::new(1);
            cx.add_node(0, vec![0.0]);
            cx.add_node(1, vec![5.0]);
            cx.add_edge(
                100,
                src,
                tgt,
                1,
                Some("test".into()),
                RestrictionMap::identity(1),
                RestrictionMap::identity(1),
                false,
            );
            cx
        };

        let forward = build(0, 1).h0_dimension();
        let reversed = build(1, 0).h0_dimension();
        assert_eq!(
            forward, reversed,
            "dim H⁰ must be invariant under edge-endpoint orientation \
             (forward {forward:?} vs reversed {reversed:?})",
        );
    }

    /// Satisfied 2-node complex: the single agreement constraint is active,
    /// so `dim H⁰ = dim ker(δ⁰)` = 1 (one connected component, 1-D stalks).
    #[test]
    fn h0_dimension_consistent_pair_is_one() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![5.0]);
        cx.add_node(1, vec![5.0]);
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("test".into()),
            RestrictionMap::identity(1),
            RestrictionMap::identity(1),
            false,
        );
        assert_eq!(cx.h0_dimension(), Some(1));
    }

    /// F2 companion (bead ley-line-open-4e8a8f): when the active system
    /// exceeds `MAX_DENSE_ELEMENTS`, `h0_dimension` must refuse to answer
    /// (`None`) rather than return `0`. `0` means "no globally consistent
    /// section exists" — semantically a WRONG ANSWER for this complex,
    /// whose all-equal stalks trivially form a consistent section.
    #[test]
    fn h0_dimension_resource_cap_is_none_not_zero() {
        // Chain of N all-equal 1-D stalks: every row of δ⁰ is active
        // (satisfied), so active_rows × ncols = (N−1) × N. Pick N so the
        // product just exceeds MAX_DENSE_ELEMENTS = 1e7.
        let n: u32 = 3_200; // 3_199 × 3_200 = 10_236_800 > 1e7
        let mut cx = CellComplex::new(1);
        for i in 0..n {
            cx.add_node(i, vec![1.0]);
        }
        for i in 0..(n - 1) {
            cx.add_edge(
                1_000_000 + i,
                i,
                i + 1,
                1,
                Some("test".into()),
                RestrictionMap::identity(1),
                RestrictionMap::identity(1),
                false,
            );
        }

        assert_eq!(
            cx.h0_dimension(),
            None,
            "resource-capped h0_dimension must be None, not a \
             valid-looking 0 (\"no consistent section\") — a trivially \
             consistent section exists here",
        );
    }

    #[test]
    fn triangle_cycle_generates_face() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![1.0]);
        cx.add_node(1, vec![2.0]);
        cx.add_node(2, vec![3.0]);

        let proj = RestrictionMap::identity(1);
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            101,
            1,
            2,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            102,
            2,
            0,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );

        let n = cx.generate_faces_from_cycles("dep", 100);
        assert_eq!(n, 1);
        assert_eq!(cx.faces.len(), 1);
    }

    #[test]
    fn cochain_complex_condition_holds_for_triangle() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![1.0]);
        cx.add_node(1, vec![2.0]);
        cx.add_node(2, vec![3.0]);

        let proj = RestrictionMap::identity(1);
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            101,
            1,
            2,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            102,
            2,
            0,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );

        cx.generate_faces_from_cycles("dep", 100);
        cx.assert_cochain_complex(); // Should not panic
    }

    // -----------------------------------------------------------------------
    // δ¹ orientation: cycles that traverse an edge against its natural
    // direction must record a -1 sign on that edge. Build a triangle whose
    // third edge is stored as 0→2 (instead of 2→0); a natural traversal
    // 0→1→2→0 must walk 0→2 in reverse and produce sign -1.
    // -----------------------------------------------------------------------

    #[test]
    fn generate_faces_from_cycles_records_negative_sign_on_reverse_edge() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![1.0]);
        cx.add_node(1, vec![1.0]);
        cx.add_node(2, vec![1.0]);

        let proj = RestrictionMap::identity(1);
        // Natural directions: 0→1, 1→2, 0→2 (note the last is NOT 2→0).
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            101,
            1,
            2,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );
        cx.add_edge(
            102,
            0,
            2,
            1,
            Some("dep".into()),
            proj.clone(),
            proj.clone(),
            false,
        );

        let created = cx.generate_faces_from_cycles("dep", 10);
        assert!(
            created >= 1,
            "expected at least one face for triangle 0→1→2→0; got {created}"
        );

        // At least one generated face must carry a -1 sign on edge 102 —
        // the cycle traverses 2→0 (reverse of stored 0→2).
        let face_with_reverse = cx.face_cells.values().find(|face| {
            face.edges
                .iter()
                .any(|&(eid, sign)| eid == 102 && sign == -1.0)
        });
        assert!(
            face_with_reverse.is_some(),
            "no generated face carries sign -1 on edge 102; faces: {:?}",
            cx.face_cells.values().map(|f| &f.edges).collect::<Vec<_>>(),
        );

        cx.assert_cochain_complex();
    }

    // -----------------------------------------------------------------------
    // add_face panic tests — fire the new validation asserts on malformed
    // input. Without these, garbage faces silently flowed into δ¹.
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "has no edges")]
    fn add_face_rejects_empty_edge_list() {
        let mut cx = CellComplex::new(1);
        cx.add_face(999, &[], 1);
    }

    #[test]
    #[should_panic(expected = "more than once")]
    fn add_face_rejects_duplicate_edges() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![1.0]);
        cx.add_node(1, vec![1.0]);
        let p = RestrictionMap::identity(1);
        cx.add_edge(100, 0, 1, 1, None, p.clone(), p, false);
        cx.add_face(999, &[(100, 1.0), (100, 1.0)], 1);
    }

    #[test]
    #[should_panic(expected = "unknown edge")]
    fn add_face_rejects_unknown_edge_id() {
        let mut cx = CellComplex::new(1);
        cx.add_face(999, &[(42, 1.0)], 1);
    }

    #[test]
    #[should_panic(expected = "do not form a cycle")]
    fn add_face_rejects_non_cycle() {
        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![1.0]);
        cx.add_node(1, vec![1.0]);
        cx.add_node(2, vec![1.0]);
        let p = RestrictionMap::identity(1);
        cx.add_edge(100, 0, 1, 1, None, p.clone(), p.clone(), false);
        cx.add_edge(101, 1, 2, 1, None, p.clone(), p, false);
        // Path 0→1→2 doesn't close back to 0.
        cx.add_face(999, &[(100, 1.0), (101, 1.0)], 1);
    }

    // -----------------------------------------------------------------------
    // enforce_transitive_closure: A→B→C path with non-identity restrictions
    // must synthesize a virtual A→C edge whose source map is f_A^{AB} and
    // target map is f_C^{BC}. Previously cloned unrelated defaults; now
    // pulls the actual stored maps along the composed path.
    // -----------------------------------------------------------------------

    #[test]
    fn enforce_transitive_closure_uses_path_endpoint_maps_not_defaults() {
        let mut cx = CellComplex::new(2);
        cx.add_node(0, vec![1.0, 0.0]);
        cx.add_node(1, vec![1.0, 0.0]);
        cx.add_node(2, vec![1.0, 0.0]);

        // Edge 0→1 uses project_dim(2, 0); edge 1→2 uses project_dim(2, 1).
        // The natural composition for A→C should use 0→1's source map on A
        // and 1→2's target map on C — distinct from each other.
        let p0 = RestrictionMap::project_dim(2, 0);
        let p1 = RestrictionMap::project_dim(2, 1);
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("dep".into()),
            p0.clone(),
            p0.clone(),
            false,
        );
        cx.add_edge(
            101,
            1,
            2,
            1,
            Some("dep".into()),
            p1.clone(),
            p1.clone(),
            false,
        );

        let virtual_id_floor = *cx
            .edges
            .iter()
            .max()
            .expect("edges non-empty when computing virtual id floor")
            + 1;
        cx.enforce_transitive_closure("dep", 1);

        // A virtual edge 0→2 must exist after closure.
        let virtual_edge_id = cx
            .edges
            .iter()
            .find(|&&eid| eid >= virtual_id_floor && cx.incidence.get(&eid) == Some(&(0u32, 2u32)))
            .copied()
            .expect("enforce_transitive_closure must synthesize a virtual 0→2 edge");

        let src_map = cx
            .restriction_maps
            .get(&(0, virtual_edge_id))
            .expect("virtual edge needs source map on node 0");
        let tgt_map = cx
            .restriction_maps
            .get(&(2, virtual_edge_id))
            .expect("virtual edge needs target map on node 2");

        // Source map mirrors p0 (from edge 100 stored as (0, 100)).
        assert_eq!(
            src_map.matrix, p0.matrix,
            "virtual src_map must equal first-edge source map"
        );
        // Target map mirrors p1 (from edge 101 stored as (2, 101)).
        assert_eq!(
            tgt_map.matrix, p1.matrix,
            "virtual tgt_map must equal last-edge target map"
        );
    }
}
