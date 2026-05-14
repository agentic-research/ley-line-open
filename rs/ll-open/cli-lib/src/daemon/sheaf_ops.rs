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

use std::collections::HashSet;
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

    fn emit(&self, topic: &str, data: serde_json::Value) {
        if let Some(ref emitter) = *self.emitter.lock().unwrap() {
            emitter.emit(topic, "leyline", data);
        }
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

    let invalidated = cache.on_change(regions);

    // Feed the co-change tracker so learned weights converge over time.
    {
        let edges = state.edges.lock().unwrap();
        let mut tracker = state.tracker.lock().unwrap();
        tracker.observe(&invalidated, &edges);
    }

    let generation = cache.generation();
    drop(cache);
    state.emit(
        "sheaf.invalidate",
        serde_json::json!({
            "invalidated": invalidated,
            "count": invalidated.len(),
            "generation": generation,
        }),
    );

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_invalidate_response::Builder = builder.init_root();
    let mut inv = root.reborrow().init_invalidated(invalidated.len() as u32);
    for (i, &r) in invalidated.iter().enumerate() {
        inv.set(i as u32, r);
    }
    root.set_count(invalidated.len() as u32);
    root.set_generation(generation);
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
        let resp = op_sheaf_invalidate(&state, &[0], &new_stalks).unwrap();
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
        let resp = op_sheaf_invalidate(&state, &[0], &new_stalks).unwrap();
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
        let resp2 = op_sheaf_invalidate(&state, &[0], &new_stalks_real).unwrap();
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
