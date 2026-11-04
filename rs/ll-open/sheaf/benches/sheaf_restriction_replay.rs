//! The live sheaf region cache against the restriction key, on the same git
//! replay (bead `ley-line-open-754aa1`).
//!
//! `leyline-cache`'s `restriction_git_replay` measures three key-strategy
//! arms. This bench runs the SAME history through the SAME driver
//! (`leyline-cache`'s `tests/common`, included by path) and adds two arms
//! backed by [`SheafCache`], the region cache the daemon serves over
//! `sheaf_set_topology` / `sheaf_invalidate` (`cli-lib/src/daemon/sheaf_ops.rs`):
//!
//! | arm | invalidation |
//! |---|---|
//! | `sheaf-heuristic` | XOR-of-Merkle-roots boundary check, BFS bounded at `HEURISTIC_CASCADE_DEPTH` (3) |
//! | `sheaf-delta0` | the same pre-filter, confirmed by per-edge ‖δ⁰‖ movement on an attached `CellComplex` |
//!
//! It lives in `leyline-sheaf` because it needs both crates: `leyline-sheaf`
//! dev-depends on `leyline-cache`, and `leyline-cache` depends on nothing in
//! `leyline-sheaf`, so there is no cycle.
//!
//! # The adapter (thin: the daemon's own topology shape)
//!
//! Per step, built from the PARENT state, as a consumer pushes topology
//! before the change arrives:
//!
//! - one region per aligned function, its stalk hash σ(function bytes);
//! - one restriction edge `F — G` when `F` refers to a token `G` defines
//!   (the call graph inside the corpus), `boundary_hash` = the XOR of the
//!   endpoints' stalk hashes, as `refresh_baseline` expects;
//! - in δ⁰ mode, an 8-dimensional f32 stalk per region (the first 8 bytes of
//!   σ(function bytes), scaled to `[0, 1]`) and `project_dim_range(8, 4)`
//!   restriction maps, the shape `op_sheaf_set_topology` builds; then
//!   `refresh_baseline`;
//! - every region's parent value `put`.
//!
//! The commit then updates every region's stalk (and f32 stalk) and calls
//! `on_change` with the regions whose bytes changed. A region still valid is
//! a HIT. The behavior of `SheafCache` is unchanged by this bench; it is
//! measured.
//!
//! The SheafCache arms do NOT claim soundness: the module doc says the
//! boundary check is a proxy that cannot see content changes outside the
//! agreement subspace, and its region graph cannot see a dependency that
//! appears only in the commit (a new def with a token `F` calls). Their false
//! skips are reported, not asserted.
//!
//! ```text
//! task cache:ablation
//! # or: cargo bench -p leyline-sheaf --bench sheaf_restriction_replay
//! ```

#[path = "../../cache/tests/common/mod.rs"]
mod common;

use common::ablation::{Arm, KeyArm, Step, Value};
use leyline_cache::strategy::{NoCache, RestrictionKey, WholeInput};
use leyline_core::ContentAddressed;
use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use std::collections::{BTreeMap, BTreeSet};

const STALK_DIM: usize = 8;
const AGREEMENT_DIM: usize = 4;
/// Edge ids share the complex's `cells` keyspace with region ids (bead
/// `ley-line-open-4fece1`); allocate them above every region id.
const EDGE_ID_BASE: u32 = 1_000_000;

struct Root([u8; 32]);

impl StalkHash for Root {
    fn merkle_root(&self) -> [u8; 32] {
        self.0
    }
}

fn root_of(bytes: &[u8]) -> [u8; 32] {
    *bytes.hash().as_bytes()
}

fn data_of(root: &[u8; 32]) -> Vec<f32> {
    root[..STALK_DIM]
        .iter()
        .map(|b| f32::from(*b) / 255.0)
        .collect()
}

/// The call graph of the parent state among the step's aligned functions.
fn call_edges(step: &Step) -> BTreeSet<(u32, u32)> {
    let mut by_def: BTreeMap<(&str, &str), Vec<u32>> = BTreeMap::new();
    for (i, t) in step.targets.iter().enumerate() {
        by_def
            .entry((t.name.as_str(), t.source_id.as_str()))
            .or_default()
            .push(i as u32);
    }
    let by_container: BTreeMap<&str, u32> = step
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.container_before.as_str(), i as u32))
        .collect();
    let mut edges = BTreeSet::new();
    for r in &step.before.refs {
        let Some(&from) = r.container.as_deref().and_then(|c| by_container.get(c)) else {
            continue;
        };
        for d in step.before.defs.iter().filter(|d| d.token == r.token) {
            for &to in by_def
                .get(&(d.token.as_str(), d.source_id.as_str()))
                .into_iter()
                .flatten()
            {
                if to != from {
                    edges.insert((from.min(to), from.max(to)));
                }
            }
        }
    }
    edges
}

struct SheafArm {
    delta_zero: bool,
    cache: SheafCache<Root, Value>,
}

impl SheafArm {
    fn new(delta_zero: bool) -> Self {
        SheafArm {
            delta_zero,
            cache: SheafCache::new(),
        }
    }
}

impl Arm for SheafArm {
    fn name(&self) -> String {
        if self.delta_zero {
            "sheaf-delta0"
        } else {
            "sheaf-heuristic"
        }
        .to_string()
    }
    fn claims_soundness(&self) -> bool {
        false
    }
    fn warm(&mut self, step: &Step) -> u64 {
        let mut cache: SheafCache<Root, Value> = SheafCache::new();
        let roots: Vec<[u8; 32]> = step
            .targets
            .iter()
            .map(|t| root_of(&t.bytes_before))
            .collect();
        let mut complex = self.delta_zero.then(|| CellComplex::new(STALK_DIM));
        for (i, t) in step.targets.iter().enumerate() {
            let id = i as u32;
            cache.set_stalk(id, Root(roots[i]));
            if let Some(cx) = complex.as_mut() {
                cx.add_node(id, data_of(&roots[i]));
            }
            cache.put(id, t.value_before.clone());
        }
        for (n, (a, b)) in call_edges(step).into_iter().enumerate() {
            let mut boundary = [0u8; 32];
            for (k, byte) in boundary.iter_mut().enumerate() {
                *byte = roots[a as usize][k] ^ roots[b as usize][k];
            }
            cache.set_restriction(
                a,
                b,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary,
                    co_change_rate: 0.0,
                    revert_rate: 0.0,
                },
            );
            if let Some(cx) = complex.as_mut() {
                let f = RestrictionMap::project_dim_range(STALK_DIM, AGREEMENT_DIM);
                cx.add_edge(
                    EDGE_ID_BASE + n as u32,
                    a,
                    b,
                    AGREEMENT_DIM,
                    Some("call".into()),
                    f.clone(),
                    f,
                    false,
                );
            }
        }
        if let Some(cx) = complex {
            cache.set_complex(cx);
            cache.refresh_baseline();
        }
        self.cache = cache;
        0
    }
    fn prepare(&mut self, step: &Step) {
        let mut changed = Vec::new();
        for (i, t) in step.targets.iter().enumerate() {
            let id = i as u32;
            let root = root_of(&t.bytes_after);
            self.cache.set_stalk(id, Root(root));
            self.cache.set_stalk_value(id, data_of(&root));
            if t.bytes_before != t.bytes_after {
                changed.push(id);
            }
        }
        self.cache.on_change(&changed);
    }
    fn lookup(&mut self, _step: &Step, i: usize) -> Option<&Value> {
        self.cache.get(&(i as u32))
    }
    fn store(&mut self, step: &Step, i: usize) {
        self.cache
            .put(i as u32, step.targets[i].value_after.clone());
    }
}

fn main() {
    let mut arms: Vec<Box<dyn Arm>> = vec![
        Box::new(KeyArm::new(NoCache)),
        Box::new(KeyArm::new(WholeInput)),
        Box::new(KeyArm::new(RestrictionKey)),
        Box::new(SheafArm::new(false)),
        Box::new(SheafArm::new(true)),
    ];
    common::git::replay_main(
        "SheafCache vs restriction key: call-target review, git replay",
        env!("CARGO_MANIFEST_DIR"),
        &mut arms,
    );
}
