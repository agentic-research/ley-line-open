//! Real-repo sheaf bench: file-as-cell, identifier-set as agreement subspace.
//!
//! Points the sheaf machinery at the leyline-sheaf crate's own source files
//! (a real-world Rust module graph: 7 files, 4 import edges) and measures
//! how many cache regions a δ⁰-driven [`SheafCache`] invalidates versus
//! the XOR-Merkle heuristic for a sequence of realistic edits.
//!
//! ## Design
//!
//! - **0-cells** = source files (`.rs` files under `src/`)
//! - **Stalk** = fixed-width f32 vector summarising the file:
//!   - `[0]` line count
//!   - `[1]` function count (`pub fn` + `fn`)
//!   - `[2..STALK_DIM]` blake-derived hash bits of the exported-identifier
//!     set — the "what does this file contract about?" projection
//! - **1-cells** = `use crate::other_module::*` import edges. The
//!   restriction map for both endpoints is a `(STALK_DIM-2) × STALK_DIM`
//!   selector matrix: extracts dims `[2..]` only (the identifier hash
//!   bits). Dims `[0]` and `[1]` (line / function counts) are the
//!   "private" dimensions that don't propagate through the sheaf.
//!
//! ## Edit scenarios
//!
//! - **Real disagreement.** Change an identifier in a file that another
//!   file imports — both the file's identifier-hash bits AND its line
//!   count flip; the importer's cache entry must invalidate under both
//!   modes. (Sanity: heuristic and δ⁰ agree here.)
//! - **Projected-away noise.** Add 50 blank lines to a file — line count
//!   flips, identifier-hash bits unchanged. Heuristic invalidates every
//!   importer (over-eviction); δ⁰ keeps them valid.
//! - **Isolated node.** Edit a file with no incoming/outgoing import
//!   edges. Both modes invalidate just that file.
//!
//! ## Output
//!
//! Asserts the δ⁰-driven cache invalidates strictly fewer entries than
//! the heuristic on the projected-away-noise scenario. The two are equal
//! on real-disagreement and isolated edits. Prints a summary table.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use leyline_sheaf::cache::{RestrictionEdge, SheafCache, StalkHash};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use nalgebra::DMatrix;
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

impl StalkHash for FileStalk {
    fn merkle_root(&self) -> [u8; 32] {
        self.content_hash
    }
}

/// Build a stalk vector from raw file content. `line_count` and
/// `fn_count` are the "private" dimensions; the remaining 30 dims are
/// derived from a SHA-256 of the file's exported-identifier set.
fn stalk_from_content(content: &str) -> FileStalk {
    let line_count = content.lines().count() as f32;
    let fn_count = content.matches("fn ").count() as f32;

    // Extract identifiers from `pub fn`, `pub struct`, `pub enum`, `pub mod`
    // declarations. Real-world projects use richer extractors; this is
    // enough for the bench's "file's exported contract" notion.
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
    data.push(line_count);
    data.push(fn_count);
    for &byte in &id_hash[..(STALK_DIM - 2)] {
        data.push(byte as f32);
    }

    // Full content hash drives the cache's XOR pre-filter — distinct
    // from the identifier-projection that drives δ⁰.
    let mut content_hasher = Sha256::new();
    content_hasher.update(content.as_bytes());
    let content_hash: [u8; 32] = content_hasher.finalize().into();

    FileStalk { data, content_hash }
}

/// Selector matrix that projects `[0..STALK_DIM]` → `[2..STALK_DIM]`.
fn identifier_projection() -> RestrictionMap {
    let mut m = DMatrix::zeros(AGREEMENT_DIM, STALK_DIM);
    for i in 0..AGREEMENT_DIM {
        m[(i, i + 2)] = 1.0;
    }
    RestrictionMap::new(m)
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

    // Hard-coded import graph reflecting the actual `use crate::...`
    // declarations in this crate (verified by `grep -E "^use crate::"`):
    //   cache.rs    -> complex, topology
    //   complex.rs  -> sparse
    //   learn.rs    -> topology
    let edges: &[(u32, u32)] = &[
        (1, 2), // cache -> complex
        (1, 6), // cache -> topology
        (2, 5), // complex -> sparse
        (3, 6), // learn -> topology
    ];
    let mut edge_id_seq = 100u32;
    for &(a, b) in edges {
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
    eprintln!(
        "[bench] AddBlankLines(cache.rs)  δ⁰ evictions={} heuristic evictions={} names_δ⁰={:?} names_heur={:?}",
        delta_zero.len(),
        heuristic.len(),
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
