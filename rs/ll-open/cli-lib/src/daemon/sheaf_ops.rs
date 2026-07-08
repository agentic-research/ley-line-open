//! Sheaf cache UDS operations.
//!
//! Surfaces [`leyline_sheaf::SheafCache`] + [`leyline_sheaf::CoChangeTracker`]
//! over the daemon's UDS + MCP wire. Consumers (mache, cloister) push
//! community topology and query structural cache invalidation.
//!
//! ## Operations
//!
//! - `sheaf_set_topology` — set community structure (regions + restriction edges)
//! - `sheaf_invalidate` — report changed regions, get structurally affected neighbors
//! - `sheaf_defect` — total boundary disagreement metric
//! - `sheaf_stalks` — per-region stalk counts
//! - `sheaf_status` — cache statistics (valid/total entries, generation, defect)
//! - `sheaf_learned_weights` — co-change-learned per-edge coupling rates
//!
//! Lifted from the private `ley-line` repo (originally `cli/src/sheaf_ops.rs`,
//! `serde_json::Value`-driven) and adapted to OSS LLO's typed [`BaseRequest`]
//! dispatch + capnp-json response builders.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use leyline_public_schema::daemon_capnp;
use leyline_sheaf::CoChangeTracker;
use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};

use super::events::EventEmitter;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Stalk backed by a raw 32-byte hash (Merkle root, xxh64 padded, etc.).
#[derive(Clone)]
pub struct HashStalk(pub [u8; 32]);

impl StalkHash for HashStalk {
    fn merkle_root(&self) -> [u8; 32] {
        self.0
    }
}

/// Shared sheaf cache state for the daemon. Lives on [`DaemonContext`]
/// as a `&'static` borrow via `Arc<SheafState>`.
pub struct SheafState {
    cache: Mutex<SheafCache<HashStalk, ()>>,
    /// Tracks co-change patterns for restriction weight learning.
    tracker: Mutex<CoChangeTracker>,
    /// Current restriction edges (needed for tracker observation;
    /// stored as canonical `(min, max)` pairs to avoid double-counting
    /// the undirected edges the cache stores in both directions).
    edges: Mutex<Vec<(u32, u32)>>,
    /// Event-bus emitter; populated by `set_emitter` at daemon init so
    /// `sheaf.topology` / `sheaf.invalidate` events flow through the
    /// ADR-010 event bus. `None` while the daemon is still wiring up.
    emitter: Mutex<Option<EventEmitter>>,
    /// Region ID → token label mapping, produced by
    /// [`crate::daemon::complex_build_pass::ComplexBuildPass`] alongside
    /// its `CellComplex` build. Load-bearing for sheaf gap 3 follow-up
    /// (bead `ley-line-open-e40566`): the watcher's fine-grained
    /// `daemon.sheaf.invalidate` diff maps `changed_files` to a subset
    /// of `region_ids` by matching labels of shape `<path>` or
    /// `<path>:sym:<NAME>` against the changed file set. Empty when no
    /// enrichment pass has produced labels yet — that's the coarse-v1
    /// fallback signal (`scope: "all-known"`).
    ///
    /// Held under its own mutex (not the cache mutex) because the
    /// watcher-driven emit reads it AFTER releasing the cache lock to
    /// keep the sheaf critical section tight. See
    /// [`SheafState::regions_touching_files`] for the read path.
    region_labels: Mutex<HashMap<u32, String>>,
}

impl Default for SheafState {
    fn default() -> Self {
        Self::new()
    }
}

impl SheafState {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(SheafCache::new()),
            tracker: Mutex::new(CoChangeTracker::default()),
            edges: Mutex::new(Vec::new()),
            emitter: Mutex::new(None),
            region_labels: Mutex::new(HashMap::new()),
        }
    }

    /// Wire the event-bus emitter in once the daemon's `EventRouter` is
    /// up. Idempotent — replaces any prior emitter.
    pub fn set_emitter(&self, emitter: EventEmitter) {
        *self.emitter.lock().unwrap() = Some(emitter);
    }

    /// Borrow the backing cache mutex. Exposed so the daemon's
    /// reparse / enrich pipelines (and e2e integration tests) can
    /// `put` cache entries directly; the `sheaf_*` UDS ops only
    /// manage topology + invalidation, not entry population.
    pub fn cache(&self) -> &Mutex<SheafCache<HashStalk, ()>> {
        &self.cache
    }

    /// Install a freshly-built [`CellComplex`] and [`CoChangeTracker`]
    /// into the shared cache. Called by [`crate::daemon::complex_build_pass::ComplexBuildPass`]
    /// at the end of its `run` so consumer queries see the derived
    /// complex without re-scanning `observation` rows on every hit.
    ///
    /// Closes the sheaf-invalidation Gap 2 (bead `ley-line-open-3af437`):
    /// previously the pass built the complex, called `build_delta_0`
    /// as a mechanical-reach witness, then dropped the complex on
    /// return. This method persists it in process memory as the
    /// module doc promises ("V1 stores the derived complex + tracker
    /// in process memory").
    ///
    /// Lock ordering: acquires `cache` and `tracker` sequentially and
    /// briefly. The cache lock is held across `refresh_baseline` so
    /// no other sheaf op observes the complex before its per-edge δ⁰
    /// baseline is captured — `refresh_baseline` is O(edges) and
    /// cheap. The tracker lock is taken after `cache` is dropped, so
    /// the two mutexes are never held simultaneously.
    pub fn install_complex(&self, complex: CellComplex, tracker: CoChangeTracker) {
        {
            let mut cache = self.cache.lock().expect("cache mutex poisoned");
            cache.set_complex(complex);
            // Snapshot the freshly-installed complex's per-edge δ⁰
            // norms as the "unchanged" reference. Without this the
            // next `on_change` would compare current state against an
            // empty baseline map and over-cascade.
            cache.refresh_baseline();
        }
        {
            let mut t = self.tracker.lock().expect("tracker mutex poisoned");
            *t = tracker;
        }
        // Reset any stale region → label mapping. A caller that has
        // labels for the freshly-installed complex must call
        // [`install_region_labels`] separately after this. Clearing
        // here is defensive — a mache-pushed topology (via
        // `op_sheaf_set_topology`) never carries labels, so if a
        // prior daemon-built complex had labels installed and mache
        // then pushed its own, the fine-grained diff would otherwise
        // map old labels to node IDs that mean something entirely
        // different in the new topology.
        {
            let mut labels = self
                .region_labels
                .lock()
                .expect("region_labels mutex poisoned");
            labels.clear();
        }
    }

    /// Install a `region_id → token label` map for the currently-
    /// installed complex. Load-bearing for sheaf gap 3 follow-up (bead
    /// `ley-line-open-e40566`): with labels installed, the watcher-
    /// driven `daemon.sheaf.invalidate` payload becomes a fine-grained
    /// diff — `region_ids` names only the regions whose labels match
    /// the `changed_files` (either as an exact path or as
    /// `<path>:sym:<NAME>`). Without labels the emit falls back to the
    /// coarse-v1 `scope: "all-known"` shape (bead
    /// `ley-line-open-3b3476`).
    ///
    /// Called by [`crate::daemon::complex_build_pass::ComplexBuildPass`]
    /// after [`install_complex`] with the observation-pass token→id
    /// map inverted to id→token — the daemon owns this mapping because
    /// it already owns the observation table that produces the
    /// labelled tokens. Consumer-pushed topologies (mache) don't touch
    /// this map; they get the fallback path by construction, matching
    /// the ADR-0026 "daemon-owned mapping" (Path A) contract.
    ///
    /// Idempotent — replaces any prior labels wholesale so a re-run of
    /// `ComplexBuildPass` never leaks a stale token that vanished from
    /// the observation table between passes.
    pub fn install_region_labels(&self, labels: HashMap<u32, String>) {
        let mut slot = self
            .region_labels
            .lock()
            .expect("region_labels mutex poisoned");
        *slot = labels;
    }

    /// Compute the fine-grained region diff for a set of `changed_files`.
    ///
    /// Returns `Some(region_ids)` when the daemon has installed labels
    /// (via [`install_region_labels`]) — the vec contains every region
    /// whose token label either equals one of `changed_files` (bare
    /// path token) or begins with `<file>:sym:` (a `path:sym:<NAME>`
    /// citation). Returns an empty vec when labels are present but no
    /// region touches the changed set — that's still a legitimate diff
    /// outcome ("nothing structural moved") and the watcher emit uses
    /// it to fire `scope: "changed-only"` with empty `region_ids` so
    /// consumers see the generation bump without over-evicting.
    ///
    /// Returns `None` when no labels are installed. That's the signal
    /// the watcher emit uses to fall back to coarse-v1 semantics
    /// (`scope: "all-known"`), matching the pre-e40566 behaviour so
    /// mache-pushed topologies and fresh-startup daemons remain
    /// backward-compatible.
    ///
    /// Cross-checks the label set against the currently-installed
    /// [`CellComplex`]: only regions whose IDs are present in the
    /// complex's `nodes` are returned. Filters out labels that
    /// survived from an earlier install if the caller forgot to
    /// clear them — belt-and-braces, since [`install_complex`]
    /// clears labels explicitly.
    ///
    /// Lock ordering: acquires `region_labels`, then `cache`
    /// sequentially, dropping each guard before the next. Callers
    /// (`emit_watcher_sheaf_invalidate`) must not be holding either
    /// mutex when they invoke this.
    pub fn regions_touching_files(&self, changed_files: &[String]) -> Option<Vec<u32>> {
        let labels = self
            .region_labels
            .lock()
            .expect("region_labels mutex poisoned");
        if labels.is_empty() {
            // No labels installed → caller falls back to `"all-known"`.
            return None;
        }
        // Filter to the current complex's node set so stale labels
        // can't leak into the payload.
        //
        // `cache.complex()?` treats "no complex installed" as "no
        // diff computable" so the caller falls back to `"all-known"`
        // with an empty region set. Labels-without-complex is a
        // degenerate state (prior complex cleared without also
        // clearing labels — shouldn't happen but the emit shouldn't
        // hallucinate region IDs from thin air).
        let live_nodes: HashSet<u32> = {
            let cache = self.cache.lock().expect("cache mutex poisoned");
            let cx = cache.complex()?;
            cx.nodes.iter().copied().collect()
        };
        // Build a `<file>:sym:` prefix set once so the O(regions ×
        // files) scan can bail on prefix match without re-allocating
        // per-region. `changed_files` empty is a valid diff — the
        // watcher fires for a topology-only change with no file scope
        // (e.g. HEAD-only movement), and every region will fail both
        // predicates so the result is an empty vec, matching the
        // "changed-only with nothing touched" contract.
        let prefixes: Vec<String> = changed_files.iter().map(|f| format!("{f}:sym:")).collect();

        let mut out: Vec<u32> = labels
            .iter()
            .filter(|(rid, _)| live_nodes.contains(rid))
            .filter_map(|(&rid, label)| {
                let matches = changed_files.iter().any(|f| f == label)
                    || prefixes.iter().any(|p| label.starts_with(p));
                if matches { Some(rid) } else { None }
            })
            .collect();
        // Sort for deterministic wire output — consumers can
        // pattern-match on ordering without depending on HashMap
        // iteration order (which changes across runs).
        out.sort_unstable();
        Some(out)
    }

    fn emit(&self, topic: &str, data: serde_json::Value) {
        if let Some(ref emitter) = *self.emitter.lock().unwrap() {
            emitter.emit(topic, "leyline", data);
        }
    }

    /// Emit `daemon.sheaf.invalidate` — the single canonical emit
    /// path for BOTH consumer-driven (`op_sheaf_invalidate`) and
    /// watcher-driven (`emit_watcher_sheaf_invalidate`) cases. Bead
    /// `ley-line-open-1104f2`.
    ///
    /// **Payload contract** (v0.6+ unified shape):
    /// - `invalidated`: `Vec<u32>` — region IDs the cascade touched
    /// - `count`: `u32` — length of `invalidated`
    /// - `scope`: `&str` — `"changed-only"` (invalidate ONLY listed regions)
    ///   or `"all-known"` (evict everything; the payload is a snapshot)
    /// - `changed_files`: `&[String]` — file scope that drove the invalidation
    ///   (empty for consumer-driven; populated for watcher-driven)
    /// - `current_root`: `String` — Σ root hex at emit time
    /// - `generation` / `prior_generation`: `u64` as JSON strings (capnp_json
    ///   convention; JS Number safe-integer ceiling)
    /// - `timestamp_ms`: `i64` as JSON string
    ///
    /// **Why `invalidated` and not `region_ids`**: matches the pre-v0.6
    /// consumer-driven emit shape and mache's `SheafInvalidateEvent.Invalidated`
    /// Go field. Renaming would break the mache wire contract without
    /// changing semantics.
    pub fn emit_invalidate(
        &self,
        invalidated: Vec<u32>,
        scope: &str,
        changed_files: &[String],
        current_root: String,
        generation: u64,
        prior_generation: u64,
    ) {
        let count = invalidated.len() as u32;
        self.emit(
            "daemon.sheaf.invalidate",
            serde_json::json!({
                "invalidated": invalidated,
                "count": count,
                "scope": scope,
                "changed_files": changed_files,
                "current_root": current_root,
                "generation": generation.to_string(),
                "prior_generation": prior_generation.to_string(),
                "timestamp_ms": super::now_ms().to_string(),
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// Request payloads (typed inputs from BaseRequest variants)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Debug, Default)]
pub struct SheafStalkInput {
    pub id: u32,
    #[serde(default)]
    pub hash: String,
    /// Optional f32 stalk vector — when non-empty AND the request's
    /// `node_stalk_dim` is > 0, the handler pushes it into the backing
    /// `CellComplex` so `detect_violations` sees the latest section.
    #[serde(default)]
    pub data: Vec<f32>,
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct SheafRestrictionInput {
    pub a: u32,
    pub b: u32,
    #[serde(default)]
    pub boundary_hash: String,
    #[serde(default)]
    pub co_change_rate: f64,
    #[serde(default)]
    pub revert_rate: f64,
    #[serde(default)]
    pub weights: Vec<f64>,
    /// Opt-in to δ⁰ mode: project the first `agreement_dim` coords of
    /// each endpoint's stalk. Zero ⇒ heuristic-only for this edge.
    #[serde(default)]
    pub agreement_dim: u32,
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct EdgeRefInput {
    pub source: u32,
    pub target: u32,
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct StalkUpdateInput {
    pub region_id: u32,
    /// New stalk values for the region. When δ⁰ mode is active, length must
    /// match the topology's `node_stalk_dim`; mismatched lengths cause the
    /// update to be skipped for that region (no panic in the handler).
    #[serde(default)]
    pub stalk: Vec<f32>,
}

/// Wire-side topology delta for the incremental `sheaf_update_topology` op.
/// Mirrors the capnp `TopologyDelta` struct field-for-field; serde decodes
/// the JSON wire payload directly into this shape via `BaseRequest`.
#[derive(serde::Deserialize, Debug, Default)]
pub struct TopologyDeltaInput {
    #[serde(default)]
    pub added_regions: Vec<SheafStalkInput>,
    #[serde(default)]
    pub removed_regions: Vec<u32>,
    #[serde(default)]
    pub added_edges: Vec<SheafRestrictionInput>,
    #[serde(default)]
    pub removed_edges: Vec<EdgeRefInput>,
    #[serde(default)]
    pub updated_stalks: Vec<StalkUpdateInput>,
}

// ---------------------------------------------------------------------------
// Handlers — one per op, each builds a capnp response and serializes to
// JSON via capnp-json.
// ---------------------------------------------------------------------------

/// Set the cache's region + restriction topology. Replaces the previous
/// stalk and restriction maps wholesale.
///
/// When `node_stalk_dim > 0` AND every region carries `data` of exactly
/// that length AND every restriction has `agreement_dim > 0`, the
/// handler also builds a backing [`CellComplex`] (with implicit
/// `project_dim_range` restriction maps), pushes the f32 stalks into
/// it, attaches it to the cache, and runs `refresh_baseline()` so the
/// δ⁰-driven invalidation contract engages on subsequent `on_change`
/// calls. The response's `delta_zero_mode` field reflects whether the
/// opt-in succeeded.
pub fn op_sheaf_set_topology(
    state: &SheafState,
    regions: &[SheafStalkInput],
    restrictions: &[SheafRestrictionInput],
    node_stalk_dim: u32,
) -> Result<String> {
    let mut cache = state.cache.lock().unwrap();

    // Try to engage δ⁰ mode: every region must carry f32 data of the
    // declared dimension and every restriction must have a non-zero
    // agreement_dim. Otherwise fall back to heuristic-only.
    let try_delta_zero = node_stalk_dim > 0
        && !regions.is_empty()
        && regions
            .iter()
            .all(|r| r.data.len() == node_stalk_dim as usize)
        && !restrictions.is_empty()
        && restrictions
            .iter()
            .all(|r| r.agreement_dim > 0 && r.agreement_dim <= node_stalk_dim);

    let mut complex_opt = if try_delta_zero {
        Some(CellComplex::new(node_stalk_dim as usize))
    } else {
        None
    };

    let mut region_count = 0u32;
    for r in regions {
        cache.set_stalk(r.id, HashStalk(parse_hash(&r.hash)));
        if let Some(cx) = complex_opt.as_mut() {
            cx.add_node(r.id, r.data.clone());
        }
        region_count += 1;
    }

    let mut edge_count = 0u32;
    let mut edge_id_seq = 100u32;
    for r in restrictions {
        let weights = if r.weights.is_empty() {
            vec![1.0]
        } else {
            r.weights.clone()
        };
        cache.set_restriction(
            r.a,
            r.b,
            RestrictionEdge {
                weights,
                boundary_hash: parse_hash(&r.boundary_hash),
                co_change_rate: r.co_change_rate,
                revert_rate: r.revert_rate,
            },
        );
        if let Some(cx) = complex_opt.as_mut() {
            let f = RestrictionMap::project_dim_range(
                node_stalk_dim as usize,
                r.agreement_dim as usize,
            );
            cx.add_edge(
                edge_id_seq,
                r.a,
                r.b,
                r.agreement_dim as usize,
                Some("daemon".into()),
                f.clone(),
                f,
                false,
            );
            edge_id_seq += 1;
        }
        edge_count += 1;
    }

    let delta_zero_mode = complex_opt.is_some();
    if let Some(cx) = complex_opt {
        cache.set_complex(cx);
        cache.refresh_baseline();
    }

    {
        let mut edges = state.edges.lock().unwrap();
        *edges = cache
            .restriction_edges()
            .map(|(&(a, b), _)| (a.min(b), a.max(b)))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
    }

    // Wipe region labels — a consumer-pushed topology (mache Louvain
    // regions) has region IDs that mean something entirely different
    // from any prior daemon-owned observation-derived labels. Leaving
    // stale labels would let the watcher-driven fine-grained diff
    // (bead `ley-line-open-e40566`) map old tokens to unrelated
    // consumer-region IDs and emit garbage. The intended behaviour
    // for a mache-pushed topology is coarse-v1 fallback
    // (`scope: "all-known"`), which is what happens when the labels
    // map is empty.
    {
        let mut labels = state.region_labels.lock().unwrap();
        labels.clear();
    }

    drop(cache);
    state.emit(
        "sheaf.topology",
        serde_json::json!({
            "regions": region_count,
            "restrictions": edge_count,
            "delta_zero_mode": delta_zero_mode,
        }),
    );

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_set_topology_response::Builder = builder.init_root();
    root.set_ok(true);
    root.set_regions(region_count);
    root.set_restrictions(edge_count);
    root.set_delta_zero_mode(delta_zero_mode);
    let reader =
        builder.get_root_as_reader::<daemon_capnp::sheaf_set_topology_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Report changed regions (optionally pushing new stalks) and run the
/// bounded BFS cascade. Returns the invalidated region list + cache
/// generation.
///
/// When δ⁰ mode is active (set_topology engaged a backing complex) and
/// a stalk carries f32 `data`, the handler pushes that data into the
/// complex via `set_stalk_value` so the next `on_change` consults the
/// real per-edge δ⁰. Hash-only updates still feed the XOR pre-filter
/// but won't move the δ⁰ baseline.
pub fn op_sheaf_invalidate(
    state: &SheafState,
    ctrl_path: &Path,
    regions: &[u32],
    stalks: &[SheafStalkInput],
) -> Result<String> {
    let mut cache = state.cache.lock().unwrap();

    for s in stalks {
        cache.set_stalk(s.id, HashStalk(parse_hash(&s.hash)));
        if !s.data.is_empty() {
            cache.set_stalk_value(s.id, s.data.clone());
        }
    }

    // Snapshot the generation BEFORE on_change bumps it so the response
    // carries the continuity tag consumers need (bead 9d5d7d).
    let prior_generation = cache.generation();
    let invalidated = cache.on_change(regions);

    // Feed the co-change tracker so learned weights converge over time.
    {
        let edges = state.edges.lock().unwrap();
        let mut tracker = state.tracker.lock().unwrap();
        tracker.observe(&invalidated, &edges);
    }

    let generation = cache.generation();
    drop(cache);

    // Bead `ley-line-open-1104f2`: route the consumer-driven emit
    // through the shared `SheafState::emit_invalidate` helper so the
    // watcher-driven path (cmd_daemon::emit_watcher_sheaf_invalidate)
    // and this consumer path emit byte-identical event shapes on the
    // same `daemon.sheaf.invalidate` topic. Payload contract lives in
    // one place; refactor drift is impossible by construction.
    //
    // Consumer-driven characteristics:
    // - `scope`: `"changed-only"` — consumer explicitly names which
    //   regions to invalidate; not a full cache snapshot.
    // - `changed_files`: empty — this op isn't tied to filesystem
    //   changes; a consumer decided which regions to invalidate on
    //   its own reasoning.
    // - `current_root`: whatever the substrate root is at emit time.
    let current_root = super::ops::read_root_hex(ctrl_path).unwrap_or_else(|e| {
        log::warn!("op_sheaf_invalidate: read_root_hex failed: {e:#}");
        String::new()
    });
    state.emit_invalidate(
        invalidated.clone(),
        "changed-only",
        &[],
        current_root,
        generation,
        prior_generation,
    );

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_invalidate_response::Builder = builder.init_root();
    let mut inv = root.reborrow().init_invalidated(invalidated.len() as u32);
    for (i, &r) in invalidated.iter().enumerate() {
        inv.set(i as u32, r);
    }
    root.set_count(invalidated.len() as u32);
    root.set_generation(generation);
    root.set_prior_generation(prior_generation);
    let reader = builder.get_root_as_reader::<daemon_capnp::sheaf_invalidate_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Total boundary disagreement summed over all restriction edges.
pub fn op_sheaf_defect(state: &SheafState) -> Result<String> {
    let cache = state.cache.lock().unwrap();
    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_defect_response::Builder = builder.init_root();
    root.set_defect(cache.defect());
    root.set_generation(cache.generation());
    root.set_valid(cache.valid_count() as u32);
    root.set_total(cache.total_count() as u32);
    let reader = builder.get_root_as_reader::<daemon_capnp::sheaf_defect_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Per-region cache validity counts (does not enumerate every region
/// hash — that's intentionally bounded to keep response size predictable).
pub fn op_sheaf_stalks(state: &SheafState) -> Result<String> {
    let cache = state.cache.lock().unwrap();
    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_stalks_response::Builder = builder.init_root();
    root.set_generation(cache.generation());
    root.set_valid(cache.valid_count() as u32);
    root.set_total(cache.total_count() as u32);
    let reader = builder.get_root_as_reader::<daemon_capnp::sheaf_stalks_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Combined cache health snapshot — defect, generation, validity, and
/// tracked-edge count for the co-change weight learner.
pub fn op_sheaf_status(state: &SheafState) -> Result<String> {
    let cache = state.cache.lock().unwrap();
    let tracker = state.tracker.lock().unwrap();
    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_status_response::Builder = builder.init_root();
    root.set_generation(cache.generation());
    root.set_valid(cache.valid_count() as u32);
    root.set_total(cache.total_count() as u32);
    root.set_defect(cache.defect());
    root.set_tracked_edges(tracker.edge_count() as u32);
    let reader = builder.get_root_as_reader::<daemon_capnp::sheaf_status_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Co-change-derived per-edge coupling rates. Empty list while no
/// invalidations have been observed.
pub fn op_sheaf_learned_weights(state: &SheafState) -> Result<String> {
    let tracker = state.tracker.lock().unwrap();
    let weights = tracker.learned_weights();
    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_learned_weights_response::Builder = builder.init_root();
    root.set_ok(true);
    root.set_edge_count(tracker.edge_count() as u32);
    let mut wlist = root.reborrow().init_weights(weights.len() as u32);
    for (i, &(a, b, rate)) in weights.iter().enumerate() {
        let mut w = wlist.reborrow().get(i as u32);
        w.set_a(a);
        w.set_b(b);
        w.set_co_change_rate(rate);
        w.set_observations(tracker.observations(a, b));
    }
    let reader =
        builder.get_root_as_reader::<daemon_capnp::sheaf_learned_weights_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// δ⁰-driven reaper — bead `ley-line-open-9c867f`, GC item 3 of the
/// sheaf-as-cache-coherence story.
///
/// Asks "given today's stalks vs the last `refresh_baseline` snapshot,
/// which cached region IDs can the consumer safely evict?" Returns a
/// structural list — this daemon never inspects the consumer's cached
/// payload. The consumer (mache, cloister, anyone) evicts the returned
/// keys and re-fetches as needed.
///
/// Companion to `sheaf_invalidate`: that op acts on caller-asserted
/// changes; reap is a pure observation. Same depth bound (radius 3
/// BFS), same per-edge `DELTA0_EPS_SQUARED` tolerance, so the two stay
/// internally consistent — a region the cascade would evict on
/// assertion is also a region the reaper would evict on observation,
/// given matching topology + stalks.
///
/// Does NOT bump the cache generation: reap is a read-only query and
/// consumers may call it multiple times during a long enrichment pass
/// without each call advancing their generation cursor.
pub fn op_sheaf_reap(state: &SheafState) -> Result<String> {
    let cache = state.cache.lock().unwrap();
    let (reclaimable, defect) = cache.reap();
    let generation = cache.generation();
    drop(cache);

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_reap_response::Builder = builder.init_root();
    let mut rlist = root.reborrow().init_reclaimable(reclaimable.len() as u32);
    for (i, &r) in reclaimable.iter().enumerate() {
        rlist.set(i as u32, r);
    }
    root.set_count(reclaimable.len() as u32);
    root.set_generation(generation);
    root.set_reaped_at_defect(defect);
    let reader = builder.get_root_as_reader::<daemon_capnp::sheaf_reap_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Apply an incremental `TopologyDelta` and re-snapshot only the affected
/// subgraph. Returns the affected region list (touched ∪ radius-1
/// neighbours) so consumers know exactly which cache entries to evict —
/// every other region is byte-identical to its pre-update value.
///
/// δ⁰ mode handling mirrors `op_sheaf_set_topology`: when a backing
/// [`CellComplex`] is attached, every shape (region add, edge add, stalk
/// update) lands in both the cache's restriction map AND the complex, then
/// `refresh_baseline_subset` re-snapshots `‖δ⁰‖²` only on edges incident to
/// the touched set. When no complex is attached, the heuristic-only path
/// applies (XOR-Merkle pre-filter as the only invalidation signal).
///
/// Lock ordering: we hold `state.cache` for the full apply-then-refresh
/// sequence so other handlers (notably `op_sheaf_invalidate`) cannot
/// observe a half-applied delta. The `state.edges` mutex is taken AFTER
/// `cache` is dropped, mirroring `op_sheaf_set_topology` to keep the lock
/// graph acyclic across all sheaf ops.
pub fn op_sheaf_update_topology(
    state: &SheafState,
    delta: &TopologyDeltaInput,
    node_stalk_dim: u32,
) -> Result<String> {
    let mut cache = state.cache.lock().unwrap();

    // 1. Build the touched-region set from the delta itself, then expand to
    //    radius-1 via the CURRENT cache topology (pre-mutation) so consumers
    //    whose entries point at a soon-to-be-detached neighbour still get
    //    that neighbour in the eviction list.
    let mut touched: BTreeSet<u32> = BTreeSet::new();
    for r in &delta.added_regions {
        touched.insert(r.id);
    }
    for &rid in &delta.removed_regions {
        touched.insert(rid);
    }
    for e in &delta.added_edges {
        touched.insert(e.a);
        touched.insert(e.b);
    }
    for e in &delta.removed_edges {
        touched.insert(e.source);
        touched.insert(e.target);
    }
    for s in &delta.updated_stalks {
        touched.insert(s.region_id);
    }

    // Pre-mutation radius-1 expansion. Removed regions' neighbours are
    // gathered here while the restriction graph still records them.
    let mut affected: BTreeSet<u32> = touched.clone();
    for &rid in &touched {
        for n in cache.neighbours(rid) {
            affected.insert(n);
        }
    }

    // 2. Apply the delta to the cache's restriction-map view.
    for e in &delta.removed_edges {
        cache.drop_restriction(e.source, e.target);
    }
    for &rid in &delta.removed_regions {
        cache.drop_region(rid);
    }
    for r in &delta.added_regions {
        cache.set_stalk(r.id, HashStalk(parse_hash(&r.hash)));
    }
    for e in &delta.added_edges {
        let weights = if e.weights.is_empty() {
            vec![1.0]
        } else {
            e.weights.clone()
        };
        cache.set_restriction(
            e.a,
            e.b,
            RestrictionEdge {
                weights,
                boundary_hash: parse_hash(&e.boundary_hash),
                co_change_rate: e.co_change_rate,
                revert_rate: e.revert_rate,
            },
        );
    }
    for s in &delta.updated_stalks {
        // Stalk overwrite: update the hash if it would change. The wire
        // doesn't carry a fresh hash field on StalkUpdate (it carries raw
        // f32 data); the cache's XOR pre-filter consults the stored hash,
        // so we leave the existing hash in place. Callers that want the
        // hash refreshed too use `sheaf_invalidate` with an explicit hash.
        // δ⁰ mode reads from the complex's stalk data we push below.
        let _ = s;
    }

    // 3. Mirror the delta into the backing CellComplex when δ⁰ mode is
    //    active. Every shape must match `node_stalk_dim` (the contract the
    //    seed `set_topology` established); regions whose data length is
    //    wrong are skipped (handler-level guard, not a panic, so a bad
    //    wire payload doesn't take the daemon down).
    if let Some(cx) = cache.complex_mut() {
        let dim = node_stalk_dim as usize;
        // Edges first so we don't drop a region while a stale edge still
        // points at it — `remove_node` on the complex already handles that
        // cascade, but explicit ordering keeps the pre/post invariants
        // independent.
        for e in &delta.removed_edges {
            cx.remove_edge(e.source, e.target);
        }
        for &rid in &delta.removed_regions {
            cx.remove_node(rid);
        }
        for r in &delta.added_regions {
            if r.data.len() == dim && dim > 0 {
                cx.add_node(r.id, r.data.clone());
            }
        }
        for e in &delta.added_edges {
            if dim == 0 || e.agreement_dim == 0 || e.agreement_dim > node_stalk_dim {
                continue;
            }
            // Skip when either endpoint isn't in the complex (heuristic-
            // only seed, or the prior delta didn't add an f32-shaped
            // region). The cache still records the restriction edge for
            // the XOR pre-filter; the complex just won't have the edge.
            if !cx.cells.contains_key(&e.a) || !cx.cells.contains_key(&e.b) {
                continue;
            }
            // Edge IDs share the `cells` HashMap namespace with regions, so
            // we start incremental-edge IDs at 1M to stay well clear of any
            // realistic region-ID range. The seed `set_topology` allocates
            // from 100 (bounded by region count), but the update op is
            // long-lived — pick a base that can't collide with future
            // region additions.
            const INCREMENTAL_EDGE_BASE: u32 = 1_000_000;
            let next_id = cx
                .edges
                .iter()
                .copied()
                .max()
                .map(|m| m + 1)
                .unwrap_or(INCREMENTAL_EDGE_BASE)
                .max(INCREMENTAL_EDGE_BASE);
            let f = RestrictionMap::project_dim_range(dim, e.agreement_dim as usize);
            cx.add_edge(
                next_id,
                e.a,
                e.b,
                e.agreement_dim as usize,
                Some("daemon".into()),
                f.clone(),
                f,
                false,
            );
        }
        for s in &delta.updated_stalks {
            if s.stalk.len() == dim && dim > 0 && cx.cells.contains_key(&s.region_id) {
                cx.set_node_stalk(s.region_id, s.stalk.clone());
            }
        }
    }

    // 4. Post-mutation radius-1 expansion. The touched set may now connect
    //    to NEW neighbours (added edges) the pre-mutation pass missed.
    for &rid in &touched {
        for n in cache.neighbours(rid) {
            affected.insert(n);
        }
    }

    // 5. Refresh the δ⁰ baseline on the affected subgraph only.
    let affected_vec: Vec<u32> = affected.iter().copied().collect();
    cache.refresh_baseline_subset(&affected_vec);

    // 6. Generation advances exactly once per update — the consumer-visible
    //    "we've moved past your snapshot" signal stays monotonic.
    //    Snapshot the prior value first so the response carries the
    //    continuity tag (bead 9d5d7d).
    let prior_generation = cache.generation();
    let generation = cache.bump_generation();
    let defect_after = cache.defect() as f32;

    // Replace the cached edge-pair list (used by op_sheaf_invalidate's
    // co-change tracker) with the post-delta canonical edge set. Dropped
    // outside the `state.edges` lock acquisition to preserve the lock
    // ordering documented above.
    let post_edges: Vec<(u32, u32)> = cache
        .restriction_edges()
        .map(|(&(a, b), _)| (a.min(b), a.max(b)))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    drop(cache);
    {
        let mut edges = state.edges.lock().unwrap();
        *edges = post_edges;
    }

    // See `op_sheaf_invalidate` above for the u64-as-string convention —
    // generation and prior_generation match capnp_json's response encoding.
    state.emit(
        "sheaf.topology",
        serde_json::json!({
            "kind": "update",
            "affected": affected_vec,
            "generation": generation.to_string(),
            "prior_generation": prior_generation.to_string(),
            "defect_after": defect_after,
        }),
    );

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_update_topology_response::Builder = builder.init_root();
    root.set_ok(true);
    root.set_generation(generation);
    root.set_prior_generation(prior_generation);
    root.set_defect_after(defect_after);
    let mut affected_list = root
        .reborrow()
        .init_affected_regions(affected_vec.len() as u32);
    for (i, &r) in affected_vec.iter().enumerate() {
        affected_list.set(i as u32, r);
    }
    let reader =
        builder.get_root_as_reader::<daemon_capnp::sheaf_update_topology_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a hex string into a 32-byte hash, zero-padding if short and
/// truncating if long. Invalid hex chars produce zero bytes — matches
/// the lenient behaviour of the private reference implementation.
fn parse_hash(hex: &str) -> [u8; 32] {
    let mut hash = [0u8; 32];
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| {
            hex.get(i..i + 2)
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect();
    let len = bytes.len().min(32);
    hash[..len].copy_from_slice(&bytes[..len]);
    hash
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_response(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("response must be valid JSON")
    }

    #[test]
    fn set_topology_then_invalidate_cascades_through_restriction_edges() {
        let state = SheafState::new();

        // 3 regions in a chain: 0 — 1 — 2, all hashes start distinct.
        let regions = vec![
            SheafStalkInput {
                id: 0,
                hash: "aa".into(),
                data: vec![],
            },
            SheafStalkInput {
                id: 1,
                hash: "bb".into(),
                data: vec![],
            },
            SheafStalkInput {
                id: 2,
                hash: "cc".into(),
                data: vec![],
            },
        ];
        let restrictions = vec![
            SheafRestrictionInput {
                a: 0,
                b: 1,
                boundary_hash: format!("{:02x}", 0xaa ^ 0xbb),
                co_change_rate: 0.5,
                revert_rate: 0.0,
                weights: vec![1.0],
                agreement_dim: 0,
            },
            SheafRestrictionInput {
                a: 1,
                b: 2,
                boundary_hash: format!("{:02x}", 0xbb ^ 0xcc),
                co_change_rate: 0.5,
                revert_rate: 0.0,
                weights: vec![1.0],
                agreement_dim: 0,
            },
        ];

        let resp = op_sheaf_set_topology(&state, &regions, &restrictions, 0).unwrap();
        let j = parse_response(&resp);
        assert_eq!(j["regions"], 3);
        assert_eq!(j["restrictions"], 2);
        assert_eq!(j["ok"], true);
        assert_eq!(j["delta_zero_mode"], false);

        // Pre-populate cache entries so on_change has something to mark.
        {
            let mut cache = state.cache.lock().unwrap();
            cache.put(0, ());
            cache.put(1, ());
            cache.put(2, ());
        }

        // Mutate region 0's stalk so the 0↔1 boundary check fails.
        let new_stalks = vec![SheafStalkInput {
            id: 0,
            hash: "ff".into(),
            data: vec![],
        }];
        let resp = op_sheaf_invalidate(
            &state,
            std::path::Path::new("/tmp/nonexistent-ctrl-for-unit-test"),
            &[0],
            &new_stalks,
        )
        .unwrap();
        let j = parse_response(&resp);
        // generation advanced
        let generation: u64 = j["generation"].as_str().unwrap().parse().unwrap();
        assert!(
            generation >= 1,
            "generation must advance after invalidate; got {generation}"
        );
        // Invalidated list contains region 0 (always) and region 1 (cascade).
        let invalidated: Vec<u32> = j["invalidated"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert!(
            invalidated.contains(&0),
            "region 0 must be invalidated; got {invalidated:?}"
        );
        assert!(
            invalidated.contains(&1),
            "region 1 must cascade-invalidate; got {invalidated:?}"
        );
    }

    #[test]
    fn defect_status_stalks_responses_have_typed_fields() {
        let state = SheafState::new();
        // Empty cache — all metrics should be zero / well-defined.
        let defect = parse_response(&op_sheaf_defect(&state).unwrap());
        assert_eq!(defect["defect"], 0.0);
        assert_eq!(defect["valid"], 0);
        assert_eq!(defect["total"], 0);

        let status = parse_response(&op_sheaf_status(&state).unwrap());
        assert_eq!(status["valid"], 0);
        assert_eq!(status["total"], 0);
        assert_eq!(status["defect"], 0.0);
        assert_eq!(status["tracked_edges"], 0);

        let stalks = parse_response(&op_sheaf_stalks(&state).unwrap());
        assert_eq!(stalks["valid"], 0);
        assert_eq!(stalks["total"], 0);
    }

    #[test]
    fn learned_weights_response_shape_with_no_observations() {
        let state = SheafState::new();
        let resp = parse_response(&op_sheaf_learned_weights(&state).unwrap());
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["edge_count"], 0);
        assert!(resp["weights"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_hash_lenient_short_zero_pads() {
        // Single byte "aa" parses to [0xaa, 0, 0, ...].
        let h = parse_hash("aa");
        assert_eq!(h[0], 0xaa);
        assert!(h[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn parse_hash_lenient_long_truncates() {
        // 33 bytes of "ff" — must truncate to 32.
        let h = parse_hash(&"ff".repeat(33));
        assert!(h.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn set_topology_engages_delta_zero_mode_when_f32_data_present() {
        // Two regions, one edge, all f32 data + agreement_dim supplied →
        // δ⁰ mode activates and the response advertises it. The cache
        // now holds a CellComplex backing the BFS cascade.
        let state = SheafState::new();
        let regions = vec![
            SheafStalkInput {
                id: 0,
                hash: "aa".into(),
                data: vec![1.0, 0.5, 0.0, 0.0],
            },
            SheafStalkInput {
                id: 1,
                hash: "bb".into(),
                data: vec![1.0, 0.5, 9.0, 9.0],
            },
        ];
        let restrictions = vec![SheafRestrictionInput {
            a: 0,
            b: 1,
            boundary_hash: format!("{:02x}", 0xaa ^ 0xbb),
            co_change_rate: 0.5,
            revert_rate: 0.0,
            weights: vec![1.0],
            agreement_dim: 2, // project first 2 coords — match on [1.0, 0.5]
        }];
        let resp = op_sheaf_set_topology(&state, &regions, &restrictions, 4).unwrap();
        let j = parse_response(&resp);
        assert_eq!(j["delta_zero_mode"], true);
        // Verify the cache has an attached complex with the right node count.
        let cache = state.cache.lock().unwrap();
        let cx = cache
            .complex()
            .expect("δ⁰ mode must attach a backing CellComplex");
        assert_eq!(cx.nodes.len(), 2);
        assert_eq!(cx.edges.len(), 1);
    }

    // ─────────────────────────────────────────────────────────────
    // Sheaf gap 3 follow-up (bead `ley-line-open-e40566`): unit
    // coverage for the fine-grained-diff accessor + its interaction
    // with topology-swap paths. The end-to-end wire is pinned by
    // tests/sheaf_gap3_invalidate_emit_test.rs; this section pins
    // the state-machine transitions themselves.
    // ─────────────────────────────────────────────────────────────

    fn seed_complex_with_ids(state: &SheafState, ids: &[u32]) {
        let mut cx = CellComplex::new(1);
        for &rid in ids {
            cx.add_node(rid, vec![0.0]);
        }
        state.install_complex(cx, CoChangeTracker::default());
    }

    #[test]
    fn regions_touching_files_returns_none_when_no_labels_installed() {
        // Coarse-v1 fallback signal: without labels the diff cannot
        // be computed; `emit_watcher_sheaf_invalidate` reads this
        // `None` as "fall back to all-known". Even installing a
        // complex must NOT satisfy the accessor — labels are the
        // load-bearing signal, not the complex.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[1, 2, 3]);
        let out = state.regions_touching_files(&["src/foo.rs".to_string()]);
        assert!(
            out.is_none(),
            "no labels installed → must return None; got {out:?}",
        );
    }

    #[test]
    fn regions_touching_files_matches_bare_path_and_sym_prefix() {
        // Load-bearing predicates: (1) label == changed_file
        // matches, (2) label starts with `changed_file:sym:`
        // matches. Regions with either shape land in the diff;
        // labels for a different file do not.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[10, 20, 30]);
        let mut labels: HashMap<u32, String> = HashMap::new();
        labels.insert(10, "src/foo.rs".to_string());
        labels.insert(20, "src/foo.rs:sym:frobnicate".to_string());
        labels.insert(30, "src/bar.rs".to_string());
        state.install_region_labels(labels);

        let mut got = state
            .regions_touching_files(&["src/foo.rs".to_string()])
            .expect("labels installed → must return Some");
        got.sort();
        assert_eq!(
            got,
            vec![10, 20],
            "bare path AND `<file>:sym:` prefix must both match; got {got:?}",
        );
    }

    #[test]
    fn regions_touching_files_returns_empty_vec_for_untouched_file() {
        // Labels installed, changed file matches nothing → Some(empty).
        // The emit path uses this to fire `scope: "changed-only"`
        // with an empty region set, honestly reporting "nothing
        // structural moved" instead of over-evicting.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[10]);
        let mut labels: HashMap<u32, String> = HashMap::new();
        labels.insert(10, "src/foo.rs".to_string());
        state.install_region_labels(labels);

        let got = state
            .regions_touching_files(&["docs/README.md".to_string()])
            .expect("labels installed → must return Some even on empty diff");
        assert!(
            got.is_empty(),
            "no label matches → empty diff, not None; got {got:?}",
        );
    }

    #[test]
    fn regions_touching_files_filters_stale_labels_against_current_complex() {
        // Belt-and-braces defence: if labels somehow reference IDs
        // not in the current complex (a broken install path, a race
        // between complex swap and label re-install), the accessor
        // must NOT hallucinate those IDs into the diff. Only region
        // IDs present in the complex's `nodes` are returned.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[10]); // complex only knows 10
        let mut labels: HashMap<u32, String> = HashMap::new();
        labels.insert(10, "src/foo.rs".to_string());
        labels.insert(999, "src/foo.rs".to_string()); // stale — not in complex
        state.install_region_labels(labels);

        let got = state
            .regions_touching_files(&["src/foo.rs".to_string()])
            .expect("labels installed → must return Some");
        assert_eq!(
            got,
            vec![10],
            "stale label for region 999 must be filtered out; got {got:?}",
        );
    }

    #[test]
    fn install_complex_clears_prior_region_labels() {
        // A fresh complex swap MUST clear the label map — otherwise
        // mache pushing its own topology via `op_sheaf_set_topology`
        // would leak stale daemon-side labels into a consumer-side
        // region-ID space, and the fine-grained diff would emit
        // garbage IDs the consumer doesn't recognize.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[10]);
        let mut labels: HashMap<u32, String> = HashMap::new();
        labels.insert(10, "src/foo.rs".to_string());
        state.install_region_labels(labels);
        // First check: labels ARE live.
        assert!(
            state
                .regions_touching_files(&["src/foo.rs".to_string()])
                .is_some()
        );

        // Now swap the complex — this must reset labels to empty.
        seed_complex_with_ids(&state, &[42]);
        assert!(
            state
                .regions_touching_files(&["src/foo.rs".to_string()])
                .is_none(),
            "install_complex must clear region_labels — otherwise stale \
             labels leak into the next topology's diff"
        );
    }

    #[test]
    fn set_topology_op_clears_prior_region_labels() {
        // Same clear invariant as `install_complex`, but for the
        // consumer-driven path. Mache's `op_sheaf_set_topology`
        // NEVER installs labels (it's the coarse fallback path by
        // design), so any residual daemon-owned labels must be
        // wiped when a consumer pushes its own topology.
        let state = SheafState::new();
        seed_complex_with_ids(&state, &[10]);
        let mut labels: HashMap<u32, String> = HashMap::new();
        labels.insert(10, "src/foo.rs".to_string());
        state.install_region_labels(labels);
        assert!(
            state
                .regions_touching_files(&["src/foo.rs".to_string()])
                .is_some()
        );

        // Push a consumer-shaped topology through the op handler.
        // Two regions, one edge; heuristic-only (no f32 data).
        let regions = vec![
            SheafStalkInput {
                id: 100,
                hash: "aa".into(),
                data: vec![],
            },
            SheafStalkInput {
                id: 200,
                hash: "bb".into(),
                data: vec![],
            },
        ];
        let restrictions = vec![SheafRestrictionInput {
            a: 100,
            b: 200,
            boundary_hash: "cc".into(),
            co_change_rate: 0.5,
            revert_rate: 0.0,
            weights: vec![1.0],
            agreement_dim: 0,
        }];
        op_sheaf_set_topology(&state, &regions, &restrictions, 0).unwrap();

        // Post-condition: labels are wiped, so the accessor falls
        // back to None — the coarse `all-known` path is engaged for
        // this consumer-pushed topology.
        assert!(
            state
                .regions_touching_files(&["src/foo.rs".to_string()])
                .is_none(),
            "op_sheaf_set_topology must clear region_labels — \
             consumer-pushed topology cannot inherit daemon-owned labels"
        );
    }

    #[test]
    fn invalidate_with_f32_data_respects_projection_subspace() {
        // δ⁰-mode topology: agreement_dim=2 projects to coords [0, 1].
        // Then invalidate region 0 with a stalk whose projection is
        // unchanged (only coord [2] differs from seed). XOR pre-filter
        // says changed, δ⁰ says unchanged → neighbor stays valid.
        let state = SheafState::new();
        let regions = vec![
            SheafStalkInput {
                id: 0,
                hash: "aa".into(),
                data: vec![1.0, 0.5, 0.0, 0.0],
            },
            SheafStalkInput {
                id: 1,
                hash: "bb".into(),
                data: vec![1.0, 0.5, 9.0, 9.0],
            },
        ];
        let restrictions = vec![SheafRestrictionInput {
            a: 0,
            b: 1,
            boundary_hash: format!("{:02x}", 0xaa ^ 0xbb),
            co_change_rate: 0.5,
            revert_rate: 0.0,
            weights: vec![1.0],
            agreement_dim: 2,
        }];
        op_sheaf_set_topology(&state, &regions, &restrictions, 4).unwrap();
        {
            let mut cache = state.cache.lock().unwrap();
            cache.put(0, ());
            cache.put(1, ());
        }

        // Change coord [2] only (private dim) — projection unchanged.
        let new_stalks = vec![SheafStalkInput {
            id: 0,
            hash: "ff".into(),
            data: vec![1.0, 0.5, 42.0, 0.0],
        }];
        let resp = op_sheaf_invalidate(
            &state,
            std::path::Path::new("/tmp/nonexistent-ctrl-for-unit-test"),
            &[0],
            &new_stalks,
        )
        .unwrap();
        let j = parse_response(&resp);
        let invalidated: Vec<u32> = j["invalidated"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert_eq!(
            invalidated,
            vec![0],
            "δ⁰ mode must hold neighbor valid when projection is unchanged; got {invalidated:?}"
        );

        // Now change coord [0] (agreement dim) — projection moves.
        let new_stalks_real = vec![SheafStalkInput {
            id: 0,
            hash: "ee".into(),
            data: vec![99.0, 0.5, 42.0, 0.0],
        }];
        // Re-mark region 1 valid before the second test.
        {
            let mut cache = state.cache.lock().unwrap();
            cache.put(1, ());
        }
        let resp2 = op_sheaf_invalidate(
            &state,
            std::path::Path::new("/tmp/nonexistent-ctrl-for-unit-test"),
            &[0],
            &new_stalks_real,
        )
        .unwrap();
        let j2 = parse_response(&resp2);
        let invalidated2: Vec<u32> = j2["invalidated"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert!(
            invalidated2.contains(&1),
            "δ⁰ mode must cascade when projection moves; got {invalidated2:?}"
        );
    }
}
