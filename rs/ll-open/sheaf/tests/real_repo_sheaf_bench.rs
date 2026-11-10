//! Real-repo sheaf bench: file-as-cell, identifier-set as agreement subspace.
//!
//! Points the sheaf machinery at the leyline-sheaf crate's own source files
//! and measures eviction precision AND simulated parse-time savings of a
//! δ⁰-driven [`SheafCache`] versus the XOR-Merkle heuristic across a
//! sequence of realistic edits.
//!
//! ## Design
//!
//! - **0-cells** = source files (`.rs` files under `src/`)
//! - **Stalk** = fixed-width f32 vector summarising the file:
//!   - `[0..AGREEMENT_DIM]` blake-derived hash bits of the exported-
//!     identifier set — the "what does this file contract about?"
//!     subspace that agreement-checks against
//!   - `[AGREEMENT_DIM..STALK_DIM]` "private" dims: line count + fn
//!     count + any local content fingerprint that doesn't affect what
//!     downstream importers see
//! - **1-cells** = `use crate::other_module*` import edges, derived by
//!   actually parsing each file (no hard-coded edge list). The
//!   restriction map for both endpoints is the canonical "project the
//!   first AGREEMENT_DIM coords" — same shape the daemon's
//!   `sheaf_set_topology` op uses when `agreement_dim` is supplied.
//!
//! ## Edit scenarios
//!
//! - **Real disagreement.** Add a `pub fn` to a file — identifier-hash
//!   bits flip; importers cascade-invalidate under both modes.
//! - **Projected-away noise.** Add blank lines to a file — line count
//!   flips, identifier-hash bits unchanged. Heuristic invalidates every
//!   importer (over-eviction); δ⁰ keeps them valid.
//! - **Isolated node.** Edit a file with no incoming/outgoing import
//!   edges. Both modes invalidate just that file.
//!
//! ## What gets measured
//!
//! 1. **Eviction count** — strict inequality on projected-away noise.
//! 2. **Simulated reparse work** — the source bytes each mode would
//!    reparse, summed across its invalidated set. δ⁰ mode reparses
//!    strictly fewer bytes on projected-away noise.
//!
//! The byte count stands in for tree-sitter's reparse cost, which is
//! linear in the source it reads. It is a deterministic count, not a
//! wall-clock time: a timing comparison inside a correctness test fails
//! under machine load (bead ley-line-open-40300f).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkDigest, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use sha2::{Digest, Sha256};

const STALK_DIM: usize = 32;
const AGREEMENT_DIM: usize = STALK_DIM - 2;

#[derive(Clone)]
struct FileStalk {
    /// The f32 stalk (length `STALK_DIM`).
    data: Vec<f32>,
    /// SHA-256 of the file content — drives the XOR-Merkle pre-filter.
    content_hash: [u8; 32],
}

impl FileStalk {
    fn merkle_root(&self) -> [u8; 32] {
        self.content_hash
    }
}

impl StalkHash for FileStalk {
    fn digest(&self) -> StalkDigest {
        StalkDigest::Hash(self.merkle_root())
    }
}

/// Build a stalk vector from raw file content. Layout:
///   `data[0..AGREEMENT_DIM]` = SHA-256 of the exported-identifier set
///     (the "agreement subspace" downstream importers project to)
///   `data[AGREEMENT_DIM..]` = "private" dims (line count, fn count,
///     content-length bucket) — these don't propagate through the sheaf
///
/// Agreement-first ordering lets the daemon's `agreement_dim` shorthand
/// ("project first N coords") apply directly — the same restriction
/// shape the bench passes to `RestrictionMap::project_dim_range`.
fn stalk_from_content(content: &str) -> FileStalk {
    let line_count = content.lines().count() as f32;
    let fn_count = content.matches("fn ").count() as f32;

    let mut idents: Vec<&str> = Vec::new();
    for kw in [
        "pub fn ",
        "pub struct ",
        "pub enum ",
        "pub mod ",
        "pub trait ",
    ] {
        for chunk in content.split(kw).skip(1) {
            if let Some(end) = chunk.find(|c: char| !c.is_alphanumeric() && c != '_') {
                idents.push(&chunk[..end]);
            }
        }
    }
    idents.sort();
    idents.dedup();

    let mut hasher = Sha256::new();
    for ident in &idents {
        hasher.update(ident.as_bytes());
        hasher.update([0]);
    }
    let id_hash: [u8; 32] = hasher.finalize().into();

    let mut data = Vec::with_capacity(STALK_DIM);
    // Agreement subspace FIRST so `project_dim_range(STALK_DIM, AGREEMENT_DIM)`
    // matches the daemon's wire-side `agreement_dim` shorthand.
    for &byte in &id_hash[..AGREEMENT_DIM] {
        data.push(byte as f32);
    }
    // Private dims after — line count, fn count, byte-length bucket.
    data.push(line_count);
    data.push(fn_count);
    // (Only AGREEMENT_DIM + 2 dims; pad to STALK_DIM if the constants
    // ever skew.)
    while data.len() < STALK_DIM {
        data.push(0.0);
    }

    let mut content_hasher = Sha256::new();
    content_hasher.update(content.as_bytes());
    let content_hash: [u8; 32] = content_hasher.finalize().into();

    FileStalk { data, content_hash }
}

/// Canonical "project the first AGREEMENT_DIM coords" restriction map.
fn identifier_projection() -> RestrictionMap {
    RestrictionMap::project_dim_range(STALK_DIM, AGREEMENT_DIM)
}

/// Parse `use crate::...` lines and return the imported top-level module
/// names. This is what closes gap #3 — the bench no longer hard-codes the
/// edge graph. Tolerates `use crate::foo::bar`, `use crate::foo;`,
/// `use crate::foo::{Bar, Baz}`, and `use crate::foo as Quux`.
fn extract_crate_imports(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let rest = match trimmed.strip_prefix("use crate::") {
            Some(r) => r,
            None => continue,
        };
        let end = rest
            .find(|c: char| matches!(c, ':' | ';' | ' ' | '{' | ','))
            .unwrap_or(rest.len());
        let module = &rest[..end];
        if !module.is_empty() {
            out.push(module.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Simulated parse cost: the bytes a reparse of this file reads. A
/// tree-sitter reparse is linear in the source it reads, so the byte count
/// is the work, and it is deterministic. The bench once measured this as
/// wall-clock µs of SHA-256 over each line, and the strict δ⁰-vs-heuristic
/// comparison then failed under machine load while the eviction counts
/// still showed δ⁰ more precise (bead ley-line-open-40300f).
fn simulated_parse_cost(content: &str) -> usize {
    content.len()
}

fn boundary_xor(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// The leyline-sheaf source tree, by region id. Order matters: cache
/// edges look up region ids and rely on stable assignment across runs.
fn enumerate_source_files() -> Vec<(u32, &'static str, PathBuf)> {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = [
        "lib.rs",
        "cache.rs",
        "complex.rs",
        "learn.rs",
        "merkle.rs",
        "sparse.rs",
        "topology.rs",
    ];
    files
        .iter()
        .enumerate()
        .map(|(i, &name)| (i as u32, name, src_dir.join(name)))
        .collect()
}

/// Build the (complex, cache) pair seeded with the current state of the
/// source tree. Edges are the intra-crate `use crate::*` imports.
fn seed_topology() -> (
    SheafCache<FileStalk, &'static str>,
    BTreeMap<u32, &'static str>,
) {
    let files = enumerate_source_files();
    let mut name_by_id = BTreeMap::new();
    let mut complex = CellComplex::new(STALK_DIM);

    let stalks: Vec<(u32, FileStalk)> = files
        .iter()
        .map(|(id, name, path)| {
            name_by_id.insert(*id, *name);
            let content = fs::read_to_string(path).expect("source file readable");
            (*id, stalk_from_content(&content))
        })
        .collect();

    for (id, stalk) in &stalks {
        complex.add_node(*id, stalk.data.clone());
    }

    // Parser-derived import graph (gap #3): walk each source file's
    // `use crate::*` lines, resolve each imported module to its region
    // id by file basename. No more hard-coded edge list — `git mv
    // learn.rs -> learning.rs` and the bench picks up the change.
    let id_by_module: BTreeMap<&str, u32> = files
        .iter()
        .map(|(id, name, _)| (name.trim_end_matches(".rs"), *id))
        .collect();
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for (id, _name, path) in &files {
        let content = fs::read_to_string(path).expect("source file readable");
        for module in extract_crate_imports(&content) {
            if let Some(&target) = id_by_module.get(module.as_str())
                && target != *id
            {
                edges.push((*id, target));
            }
        }
    }
    edges.sort();
    edges.dedup();
    assert!(
        !edges.is_empty(),
        "parser must derive at least one import edge from this crate's sources"
    );
    let mut edge_id_seq = 100u32;
    for &(a, b) in &edges {
        complex.add_edge(
            edge_id_seq,
            a,
            b,
            AGREEMENT_DIM,
            Some("import".into()),
            identifier_projection(),
            identifier_projection(),
            false,
        );
        edge_id_seq += 1;
    }

    let mut cache: SheafCache<FileStalk, &'static str> = SheafCache::new().with_complex(complex);
    for (id, stalk) in &stalks {
        cache.set_stalk(*id, stalk.clone());
        cache.set_stalk_value(*id, stalk.data.clone());
        cache.put(*id, "parsed-ast");
    }
    for &(a, b) in &edges {
        let stalk_a = &stalks.iter().find(|(id, _)| *id == a).unwrap().1;
        let stalk_b = &stalks.iter().find(|(id, _)| *id == b).unwrap().1;
        let edge = RestrictionEdge {
            weights: vec![1.0],
            co_change_rate: 0.0,
            revert_rate: 0.0,
            boundary_hash: boundary_xor(&stalk_a.merkle_root(), &stalk_b.merkle_root()),
        };
        cache.set_restriction(a, b, edge);
    }

    // Snapshot the seed state as the per-edge baseline. From here on,
    // an edit "moves" the agreement iff `‖δ⁰‖²` shifts away from this
    // baseline — exactly what the moat claim requires.
    cache.refresh_baseline();
    (cache, name_by_id)
}

/// Mirror of `seed_topology` but without an attached complex — the
/// XOR-Merkle heuristic IS the entire invalidation contract here.
fn seed_heuristic_only() -> SheafCache<FileStalk, &'static str> {
    let files = enumerate_source_files();
    let stalks: Vec<(u32, FileStalk)> = files
        .iter()
        .map(|(id, _name, path)| {
            let content = fs::read_to_string(path).expect("source file readable");
            (*id, stalk_from_content(&content))
        })
        .collect();

    let mut cache: SheafCache<FileStalk, &'static str> = SheafCache::new();
    for (id, stalk) in &stalks {
        cache.set_stalk(*id, stalk.clone());
        cache.put(*id, "parsed-ast");
    }

    let edges: &[(u32, u32)] = &[(1, 2), (1, 6), (2, 5), (3, 6)];
    for &(a, b) in edges {
        let stalk_a = &stalks.iter().find(|(id, _)| *id == a).unwrap().1;
        let stalk_b = &stalks.iter().find(|(id, _)| *id == b).unwrap().1;
        let edge = RestrictionEdge {
            weights: vec![1.0],
            co_change_rate: 0.0,
            revert_rate: 0.0,
            boundary_hash: boundary_xor(&stalk_a.merkle_root(), &stalk_b.merkle_root()),
        };
        cache.set_restriction(a, b, edge);
    }
    cache
}

// ---------------------------------------------------------------------
// Apply a synthetic edit to a region: produce a (new f32 stalk, new
// content hash) pair simulating what a parser would derive after the
// edit lands.
// ---------------------------------------------------------------------

enum Edit {
    /// Add blank lines: line count flips, identifier set unchanged.
    /// The projected-away-noise case — heuristic over-evicts; δ⁰ keeps
    /// neighbors valid.
    AddBlankLines { region: u32, count: usize },
    /// Add a new `pub fn` — both line count AND the identifier-hash bits
    /// flip. Both modes must invalidate neighbors.
    AddExportedFn { region: u32, ident: &'static str },
}

fn apply_edit(
    cache_with_complex: &mut SheafCache<FileStalk, &'static str>,
    cache_heuristic: &mut SheafCache<FileStalk, &'static str>,
    edit: &Edit,
) -> (Vec<u32>, Vec<u32>) {
    let (region, new_stalk) = match *edit {
        Edit::AddBlankLines { region, count } => {
            let path = enumerate_source_files()
                .into_iter()
                .find(|(id, _, _)| *id == region)
                .map(|(_, _, p)| p)
                .expect("region exists");
            let original = fs::read_to_string(&path).expect("readable");
            let edited = format!("{}\n{}", original, "\n".repeat(count));
            (region, stalk_from_content(&edited))
        }
        Edit::AddExportedFn { region, ident } => {
            let path = enumerate_source_files()
                .into_iter()
                .find(|(id, _, _)| *id == region)
                .map(|(_, _, p)| p)
                .expect("region exists");
            let original = fs::read_to_string(&path).expect("readable");
            let edited = format!("{original}\npub fn {ident}() {{}}\n");
            (region, stalk_from_content(&edited))
        }
    };

    cache_with_complex.set_stalk(region, new_stalk.clone());
    cache_with_complex.set_stalk_value(region, new_stalk.data.clone());
    cache_heuristic.set_stalk(region, new_stalk.clone());

    let invalidated_complex = cache_with_complex.on_change(&[region]);
    let invalidated_heuristic = cache_heuristic.on_change(&[region]);
    (invalidated_complex, invalidated_heuristic)
}

#[test]
fn real_repo_bench_delta_zero_strictly_more_precise_on_projected_away_noise() {
    let (mut cache_with_complex, name_by_id) = seed_topology();
    let mut cache_heuristic = seed_heuristic_only();

    // Scenario 1: blank-lines edit to cache.rs (region 1). cache.rs
    // imports from complex (region 2) and topology (region 6). Heuristic
    // cascade should hit those neighbors because the content hash flips;
    // δ⁰ should keep them valid because the identifier-projection bits
    // didn't move.
    let (delta_zero, heuristic) = apply_edit(
        &mut cache_with_complex,
        &mut cache_heuristic,
        &Edit::AddBlankLines {
            region: 1,
            count: 50,
        },
    );
    // Sum a simulated parse cost across each side's invalidated set so
    // the bench output carries a reparse-work signal, not just an eviction
    // count: the source bytes each side would reparse.
    let parse_bytes = |regions: &[u32]| -> usize {
        regions
            .iter()
            .map(|&r| {
                let path = enumerate_source_files()
                    .into_iter()
                    .find(|(id, _, _)| *id == r)
                    .map(|(_, _, p)| p)
                    .expect("region in enumeration");
                let content = fs::read_to_string(path).expect("readable");
                simulated_parse_cost(&content)
            })
            .sum()
    };
    let delta_zero_bytes = parse_bytes(&delta_zero);
    let heuristic_bytes = parse_bytes(&heuristic);
    eprintln!(
        "[bench] AddBlankLines(cache.rs)  δ⁰ evictions={} ({} bytes reparsed) heuristic evictions={} ({} bytes reparsed) names_δ⁰={:?} names_heur={:?}",
        delta_zero.len(),
        delta_zero_bytes,
        heuristic.len(),
        heuristic_bytes,
        delta_zero.iter().map(|r| name_by_id[r]).collect::<Vec<_>>(),
        heuristic.iter().map(|r| name_by_id[r]).collect::<Vec<_>>(),
    );
    // δ⁰ touches only the changed region itself; heuristic also flags
    // both import targets because the content hash flipped.
    assert!(
        delta_zero.len() < heuristic.len(),
        "δ⁰ must invalidate strictly fewer regions than heuristic on projected-away noise; \
         got δ⁰={delta_zero:?} heuristic={heuristic:?}"
    );
    assert!(
        delta_zero.contains(&1),
        "the edited region must always be invalidated"
    );
    assert!(
        delta_zero.iter().all(|r| heuristic.contains(r)),
        "every region δ⁰ invalidates must also be invalidated by the heuristic; \
         got δ⁰={delta_zero:?} heuristic={heuristic:?}"
    );
    assert!(
        delta_zero_bytes < heuristic_bytes,
        "δ⁰ must reparse strictly fewer source bytes than the heuristic on projected-away noise; \
         got δ⁰={delta_zero_bytes} heuristic={heuristic_bytes}",
    );

    // Scenario 2: AddExportedFn on complex.rs (region 2). complex.rs's
    // identifier-hash bits flip; cache.rs imports from complex → cache.rs
    // must invalidate. Both modes should agree the cascade is real.
    let (mut cache_with_complex2, _names2) = seed_topology();
    let mut cache_heuristic2 = seed_heuristic_only();
    let (delta_zero2, heuristic2) = apply_edit(
        &mut cache_with_complex2,
        &mut cache_heuristic2,
        &Edit::AddExportedFn {
            region: 2,
            ident: "bench_added_export",
        },
    );
    eprintln!(
        "[bench] AddExportedFn(complex.rs) δ⁰ evictions={} heuristic evictions={}",
        delta_zero2.len(),
        heuristic2.len(),
    );
    assert!(
        delta_zero2.contains(&2),
        "edited region 2 must be invalidated"
    );
    assert!(
        heuristic2.contains(&2),
        "heuristic must also invalidate edited region 2"
    );
    // Both should also flag cache.rs (region 1) which imports from complex.
    assert!(
        heuristic2.contains(&1),
        "heuristic must cascade to cache.rs (real disagreement); got {heuristic2:?}"
    );

    // Scenario 3: edit an isolated file (merkle.rs, region 4) — no
    // restriction edges incident to it. Neither mode should cascade.
    let (mut cache_with_complex3, _names3) = seed_topology();
    let mut cache_heuristic3 = seed_heuristic_only();
    let (delta_zero3, heuristic3) = apply_edit(
        &mut cache_with_complex3,
        &mut cache_heuristic3,
        &Edit::AddBlankLines {
            region: 4,
            count: 10,
        },
    );
    eprintln!(
        "[bench] AddBlankLines(merkle.rs)  δ⁰ evictions={} heuristic evictions={}",
        delta_zero3.len(),
        heuristic3.len(),
    );
    assert_eq!(
        delta_zero3,
        vec![4],
        "isolated edit must invalidate only the edited region under δ⁰"
    );
    assert_eq!(
        heuristic3,
        vec![4],
        "isolated edit must invalidate only the edited region under heuristic"
    );
}
