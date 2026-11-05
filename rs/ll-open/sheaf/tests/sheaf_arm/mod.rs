//! The [`SheafCache`] arms of the restriction-cache ablation (beads
//! `ley-line-open-754aa1`, `ley-line-open-f04610`), shared by the
//! `sheaf_restriction_replay` bench and the `sheaf_cache_advisory` regression
//! test so both drive the cache through ONE adapter.
//!
//! The includer must declare `mod common;` at its crate root, pointing at
//! `leyline-cache`'s `tests/common/mod.rs` (the replay driver).
//!
//! # The adapter (the daemon's own topology shape)
//!
//! Per step, built from the PARENT state, as a consumer pushes topology
//! before the change arrives:
//!
//! - one region per aligned function, its stalk hash σ(function bytes);
//! - one restriction edge `F — G` when `F` refers to a token `G` defines
//!   (the call graph inside the corpus), `boundary_hash` = the XOR of the
//!   endpoints' stalk hashes, as `refresh_baseline` expects;
//! - in δ⁰ mode, an 8-dimensional f32 stalk per region with
//!   `project_dim_range(8, 4)` restriction maps, the shape
//!   `op_sheaf_set_topology` builds, then `refresh_baseline`;
//! - every region's parent value `put`.
//!
//! The commit then updates every region's stalk (and f32 stalk) and calls
//! `on_change` with the regions whose bytes changed. A region whose entry is
//! still valid is a HIT.
//!
//! # The δ⁰ stalk carries agreement-subspace structure
//!
//! mache's `ComputeStalk` (the only producer of f32 stalks over the wire)
//! puts a digest of the region's cross-region interface in the agreement
//! coordinates and region-private features in the rest, so a change that
//! leaves the interface alone projects to zero across every edge. The δ⁰ arm
//! does the same over a function region:
//!
//! - coordinates `0..4` (the agreement subspace `project_dim_range` keeps):
//!   σ(function name ‖ the sorted `(token, qualifier)` of its call refs),
//!   the interface its neighbours see;
//! - coordinates `4..8` (projected away): σ(function bytes), the body.
//!
//! A body-only edit therefore leaves every incident edge's ‖δ⁰‖ at its
//! baseline and stops the cascade at the root; an edit that changes what the
//! function calls moves the norm and propagates. That is the distinction the
//! δ⁰ gate exists to make, and a byte-hash stalk in every coordinate (the
//! adapter's first version) could not exercise it.

use crate::common::ablation::{Arm, Step, Value};
use leyline_core::ContentAddressed;
use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use std::collections::{BTreeMap, BTreeSet};

pub const STALK_DIM: usize = 8;
pub const AGREEMENT_DIM: usize = 4;
/// Edge ids share the complex's `cells` keyspace with region ids (bead
/// `ley-line-open-4fece1`); allocate them above every region id.
const EDGE_ID_BASE: u32 = 1_000_000;

pub struct Root(pub [u8; 32]);

impl StalkHash for Root {
    fn merkle_root(&self) -> [u8; 32] {
        self.0
    }
}

pub fn root_of(bytes: &[u8]) -> [u8; 32] {
    *bytes.hash().as_bytes()
}

fn unit(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes.iter().map(|b| f32::from(*b) / 255.0)
}

/// The δ⁰ stalk of one function region: interface digest in the agreement
/// coordinates, body digest in the private ones (see the module doc).
pub fn interface_stalk(
    sub: &leyline_cache::restriction::FactSubstrate,
    container: &str,
    name: &str,
    bytes: &[u8],
) -> Vec<f32> {
    let mut calls: Vec<(&str, &str)> = sub
        .refs
        .iter()
        .filter(|r| r.container.as_deref() == Some(container))
        .map(|r| (r.token.as_str(), r.qualifier.as_deref().unwrap_or("")))
        .collect();
    calls.sort_unstable();
    calls.dedup();
    let mut interface = Vec::new();
    interface.extend_from_slice(name.as_bytes());
    for (token, qualifier) in calls {
        interface.push(0);
        interface.extend_from_slice(qualifier.as_bytes());
        interface.push(b':');
        interface.extend_from_slice(token.as_bytes());
    }
    let agreement = root_of(&interface);
    let body = root_of(bytes);
    unit(&agreement[..AGREEMENT_DIM])
        .chain(unit(&body[..STALK_DIM - AGREEMENT_DIM]))
        .collect()
}

/// The call graph of the parent state among the step's aligned functions.
pub fn call_edges(step: &Step) -> BTreeSet<(u32, u32)> {
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

pub struct SheafArm {
    delta_zero: bool,
    pub cache: SheafCache<Root, Value>,
}

impl SheafArm {
    pub fn new(delta_zero: bool) -> Self {
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
    /// `SheafCache` is advisory (its module doc, bead
    /// `ley-line-open-f04610`): its false skips are reported, never
    /// asserted to be zero.
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
                cx.add_node(
                    id,
                    interface_stalk(&step.before, &t.container_before, &t.name, &t.bytes_before),
                );
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
            self.cache.set_stalk(id, Root(root_of(&t.bytes_after)));
            if self.delta_zero {
                self.cache.set_stalk_value(
                    id,
                    interface_stalk(&step.after, &t.container_after, &t.name, &t.bytes_after),
                );
            }
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
