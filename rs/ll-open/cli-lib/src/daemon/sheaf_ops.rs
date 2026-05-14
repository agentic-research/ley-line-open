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
}

// ---------------------------------------------------------------------------
// Handlers — one per op, each builds a capnp response and serializes to
// JSON via capnp-json.
// ---------------------------------------------------------------------------

/// Set the cache's region + restriction topology. Replaces the previous
/// stalk and restriction maps wholesale.
pub fn op_sheaf_set_topology(
    state: &SheafState,
    regions: &[SheafStalkInput],
    restrictions: &[SheafRestrictionInput],
) -> Result<String> {
    let mut cache = state.cache.lock().unwrap();

    let mut region_count = 0u32;
    for r in regions {
        let hash = parse_hash(&r.hash);
        cache.set_stalk(r.id, HashStalk(hash));
        region_count += 1;
    }

    let mut edge_count = 0u32;
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
        edge_count += 1;
    }

    // Update tracked edge list (canonical (min,max) pairs) for co-change
    // observations.
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
        }),
    );

    let mut builder = capnp::message::Builder::new_default();
    let mut root: daemon_capnp::sheaf_set_topology_response::Builder = builder.init_root();
    root.set_ok(true);
    root.set_regions(region_count);
    root.set_restrictions(edge_count);
    let reader =
        builder.get_root_as_reader::<daemon_capnp::sheaf_set_topology_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Report changed regions (optionally pushing new stalks) and run the
/// bounded BFS cascade. Returns the invalidated region list + cache
/// generation.
pub fn op_sheaf_invalidate(
    state: &SheafState,
    regions: &[u32],
    stalks: &[SheafStalkInput],
) -> Result<String> {
    let mut cache = state.cache.lock().unwrap();

    for s in stalks {
        cache.set_stalk(s.id, HashStalk(parse_hash(&s.hash)));
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
            },
            SheafStalkInput {
                id: 1,
                hash: "bb".into(),
            },
            SheafStalkInput {
                id: 2,
                hash: "cc".into(),
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
            },
            SheafRestrictionInput {
                a: 1,
                b: 2,
                boundary_hash: format!("{:02x}", 0xbb ^ 0xcc),
                co_change_rate: 0.5,
                revert_rate: 0.0,
                weights: vec![1.0],
            },
        ];

        let resp = op_sheaf_set_topology(&state, &regions, &restrictions).unwrap();
        let j = parse_response(&resp);
        assert_eq!(j["regions"], 3);
        assert_eq!(j["restrictions"], 2);
        assert_eq!(j["ok"], true);

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
}
