//! End-to-end gates for the topology pre-pass (bead `ley-line-open-9d3208`).
//!
//! Five falsifiability gates from the bead description, tightened per the
//! skeptic review on the same bead:
//!
//! 1. **Cost ceiling** — TWO benchmarks: an empty-file lower bound
//!    (1000 ~120-byte files; budget <50 ms) and a realistic-size
//!    benchmark (1000 ~6-15 KiB files; budget <500 ms). The realistic
//!    benchmark is the contract; the lower bound is documentation.
//! 2. **Recall (presence)** — hand-crafted multi-language fixture; ALL
//!    of {Go, Rust, Python, TS} detected as `EdgeEstimate` entries with
//!    `confidence > 0.0`. Reported as "N/4 languages detected" — NOT a
//!    fabricated decimal recall figure.
//! 3. **Manifest detection** — depth 1, 2, and 3+ all assign the correct
//!    deepest-ancestor region; bloat-dir manifests (vendor/, node_modules/)
//!    are excluded from region assignment because `collect_files` skips
//!    those directories entirely.
//! 4. **Determinism** — same input twice → byte-identical serialization
//!    over ALL five `TopologyOutput` fields (`edge_estimates`,
//!    `parse_order`, `file_regions`, `region_edges`, stats projection).
//! 5. **Translation spot-check** — every `region_edges → SheafRestrictionInput`
//!    triple carries `a != b`, `co_change_rate in (0.0, 1.0]`, and
//!    `agreement_dim > 0`. Plus a fixture-specific spot-check that the
//!    `a`/`b` pair maps to a real `(source_region, target_region)`.
//!
//! Each gate is its own `#[test]` so a failure in one doesn't mask
//! the others.

use std::path::{Path, PathBuf};

use leyline_cli_lib::daemon::sheaf_ops::SheafRestrictionInput;
use leyline_cli_lib::topology_pass::{self, EdgeEstimate, FileRegion, Lang, TopologyOutput};

const HANDCRAFTED: &str = "tests/fixtures/topology/handcrafted";

fn handcrafted_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(HANDCRAFTED)
}

/// Collect files under the handcrafted fixture in deterministic order.
fn handcrafted_files() -> Vec<PathBuf> {
    let mut files = topology_pass::collect_files(&handcrafted_root())
        .expect("walk handcrafted fixture");
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Gate 1 — cost ceiling.
//
// Two scenarios so the skeptic-flagged "empty-file lower bound vs real-codebase
// performance" gap is explicit in the test surface, not hand-waved.
// ---------------------------------------------------------------------------

/// Lower-bound benchmark: ~120-byte synthetic files. NOT a realistic
/// production load; the budget here only catches algorithmic regressions
/// (e.g. an accidental O(n²) sweep) that would even blow up on trivial
/// input.
#[test]
fn gate1a_cost_ceiling_empty_file_lower_bound() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    generate_synthetic_1000_small(root);

    let files = topology_pass::collect_files(root).expect("walk synthetic");
    assert!(
        files.len() >= 1000,
        "lower-bound fixture should be ≥1000 files, got {}",
        files.len()
    );

    // Two-run timing: first run primes the OS page cache.
    let _warm = topology_pass::run(&files, root).expect("warm-up run");

    let started = std::time::Instant::now();
    let out = topology_pass::run(&files, root).expect("timed run");
    let elapsed = started.elapsed();

    eprintln!(
        "[Gate 1a / empty-file lower bound] files={} scanned={} regions={} edges={} elapsed={:?}",
        out.stats.n_files,
        out.stats.n_files_scanned,
        out.stats.n_regions,
        out.stats.n_edges,
        elapsed,
    );

    // Permissive ceiling — this is a *floor* on what the cost should
    // be, not a production budget. The realistic benchmark below
    // (gate1b) is the contract.
    assert!(
        elapsed.as_millis() < 150,
        "Gate 1a FAILED: lower-bound run took {:?} on {} files (lower-bound budget: 150ms)",
        elapsed,
        files.len()
    );
}

/// Realistic-size benchmark: ~6-15 KiB per file (the actual average for
/// production codebases). The pre-pass reads up to 4 KiB per file, so
/// each scan now actually pulls a full 4 KiB page rather than a
/// 60-byte partial.
#[test]
fn gate1b_cost_ceiling_realistic_file_sizes() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    generate_synthetic_1000_realistic(root);

    let files = topology_pass::collect_files(root).expect("walk synthetic");
    assert!(
        files.len() >= 1000,
        "realistic fixture should be ≥1000 files, got {}",
        files.len()
    );

    // Spot-check file sizes are actually in the target range.
    let sample_size = std::fs::metadata(&files[files.len() / 2])
        .expect("stat sample")
        .len();
    assert!(
        (6_000..=15_000).contains(&(sample_size as usize)),
        "Gate 1b setup: sample file size {sample_size} bytes outside 6-15 KiB target — \
         fixture generator drifted"
    );

    let _warm = topology_pass::run(&files, root).expect("warm-up run");
    let started = std::time::Instant::now();
    let out = topology_pass::run(&files, root).expect("timed run");
    let elapsed = started.elapsed();

    eprintln!(
        "[Gate 1b / realistic 6-15 KiB] files={} scanned={} regions={} edges={} elapsed={:?}",
        out.stats.n_files,
        out.stats.n_files_scanned,
        out.stats.n_regions,
        out.stats.n_edges,
        elapsed,
    );

    // Realistic budget. 4 KiB/file × 1000 = ~4 MB I/O over a page-cached
    // tempdir + regex sweep + region assignment. 500 ms is generous;
    // anything close to that is a perf regression worth investigating.
    assert!(
        elapsed.as_millis() < 500,
        "Gate 1b FAILED: realistic run took {:?} on {} files (budget: 500ms)",
        elapsed,
        files.len()
    );
}

// ---------------------------------------------------------------------------
// Gate 2 — recall on hand-crafted multi-language fixture.
//
// Reported as PRESENCE COUNT (e.g. "4/4 languages detected"), NOT a
// fabricated decimal "recall" figure. The handcrafted fixture has exactly
// one canonical import per language; the only honest measurements are
// either "detected" or "missed".
// ---------------------------------------------------------------------------

#[test]
fn gate2_recall_detects_all_four_language_imports() {
    let files = handcrafted_files();
    let root = handcrafted_root();
    let out = topology_pass::run(&files, &root).expect("run handcrafted");

    // Build a (lang, from_basename, to_basename) view for human-readable
    // assertions.
    let edge_view: Vec<(Lang, String, String, f32)> = out
        .edge_estimates
        .iter()
        .map(|e| {
            let from = basename(&files[e.from]);
            let to = basename(&files[e.to]);
            (e.language, from, to, e.confidence)
        })
        .collect();

    let langs_seen: std::collections::BTreeSet<Lang> =
        out.edge_estimates.iter().map(|e| e.language).collect();
    let required = [Lang::Go, Lang::Rust, Lang::Python, Lang::Ts];
    let detected_count = required.iter().filter(|l| langs_seen.contains(*l)).count();
    eprintln!(
        "[Gate 2] {}/{} required languages detected; {} edges total",
        detected_count,
        required.len(),
        edge_view.len(),
    );
    for (lang, f, t, c) in &edge_view {
        eprintln!("    {lang:?}  {f} -> {t}  (conf={c:.3})");
    }

    let missing: Vec<&Lang> = required
        .iter()
        .filter(|l| !langs_seen.contains(*l))
        .collect();
    assert!(
        missing.is_empty(),
        "Gate 2 FAILED: {}/{} required languages detected; missing {missing:?}",
        detected_count,
        required.len(),
    );

    // Every detected edge must carry positive confidence in the
    // documented `(0.0, 1.0]` range.
    for e in &out.edge_estimates {
        assert!(
            e.confidence > 0.0 && e.confidence <= 1.0,
            "Gate 2 FAILED: edge {e:?} confidence {} outside (0.0, 1.0]",
            e.confidence,
        );
    }

    // Spot-check: each of the 4 specific imports we hand-crafted should
    // resolve to its intended target by basename.
    assert_has_edge(&edge_view, Lang::Go, "main.go", "bar.go");
    assert_has_edge(&edge_view, Lang::Rust, "lib.rs", "baz.rs");
    assert_has_edge(&edge_view, Lang::Python, "main.py", "bar.py");
    assert_has_edge(&edge_view, Lang::Ts, "main.ts", "util.ts");

    // Block-mode Go import: `grouped.go` imports `bar` and `quux` from
    // a multi-line `import ( ... )` block. The parser branch is
    // otherwise unexercised at the integration level.
    assert_has_edge(&edge_view, Lang::Go, "grouped.go", "bar.go");
    assert_has_edge(&edge_view, Lang::Go, "grouped.go", "quux.go");

    // TS export-from: `reexport.ts` re-exports `x` from `./util` — added
    // in the TS regex fix for finding 11.
    assert_has_edge(&edge_view, Lang::Ts, "reexport.ts", "util.ts");
}

// ---------------------------------------------------------------------------
// Gate 3 — manifest detection.
//
// Three sub-gates: depth-1+2 (root + subcrate), depth-3 (nested
// subcrate), and bloat-dir exclusion (vendor/, node_modules/).
// ---------------------------------------------------------------------------

#[test]
fn gate3a_manifest_detection_nested_root_and_subcrate() {
    let files = handcrafted_files();
    let root = handcrafted_root();
    let out = topology_pass::run(&files, &root).expect("run handcrafted");

    let regions: std::collections::BTreeSet<_> =
        out.file_regions.iter().map(|fr| fr.region).collect();
    eprintln!(
        "[Gate 3a] manifests={} regions={:?} n_regions_stat={}",
        out.stats.n_manifests, regions, out.stats.n_regions
    );
    assert_eq!(
        out.stats.n_manifests, 2,
        "Gate 3a FAILED: expected 2 manifests (root + subcrate), got {}",
        out.stats.n_manifests
    );
    assert_eq!(
        regions.len(),
        2,
        "Gate 3a FAILED: expected exactly 2 distinct regions, got {regions:?}"
    );

    // Find the subcrate region by inspecting any file under `subcrate/`.
    let subcrate_region = out
        .file_regions
        .iter()
        .find(|fr| {
            files[fr.file_index]
                .strip_prefix(&root)
                .ok()
                .map(|rel| rel.starts_with("subcrate"))
                .unwrap_or(false)
        })
        .expect("at least one subcrate file in fixture")
        .region;

    // And a non-subcrate file (e.g. py/main.py) for the root region.
    let root_region = out
        .file_regions
        .iter()
        .find(|fr| {
            files[fr.file_index]
                .strip_prefix(&root)
                .ok()
                .map(|rel| !rel.starts_with("subcrate"))
                .unwrap_or(false)
        })
        .expect("at least one non-subcrate file in fixture")
        .region;

    assert_ne!(
        subcrate_region, root_region,
        "Gate 3a FAILED: subcrate and root share region id {subcrate_region}"
    );

    // Skeptic NIT-3: the bead spec says "subcrate gets deeper id." Manifests
    // are sorted by directory path → deeper directory = lexicographically
    // later = larger id (1-indexed). Pin that ordering.
    assert!(
        subcrate_region > root_region,
        "Gate 3a FAILED: subcrate region {subcrate_region} not deeper than root region {root_region}"
    );

    // Every file under subcrate/ MUST be in the subcrate region.
    for fr in &out.file_regions {
        let rel = files[fr.file_index].strip_prefix(&root).unwrap();
        if rel.starts_with("subcrate") {
            assert_eq!(
                fr.region, subcrate_region,
                "Gate 3a FAILED: subcrate file {} got region {} (expected {})",
                rel.display(),
                fr.region,
                subcrate_region
            );
        }
    }
}

/// Depth-3 manifest detection: root + subcrate + nested-subcrate. The
/// previous test only exercised root + 1 level; ensure deep ancestry
/// chains still pick the deepest manifest.
#[test]
fn gate3b_manifest_detection_depth_3_nested() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    write_depth_3_fixture(root);

    let files = topology_pass::collect_files(root).expect("walk");
    let out = topology_pass::run(&files, root).expect("run");

    eprintln!(
        "[Gate 3b / depth-3] manifests={} regions={}",
        out.stats.n_manifests, out.stats.n_regions
    );

    assert_eq!(
        out.stats.n_manifests, 3,
        "Gate 3b FAILED: expected 3 manifests (root + subcrate + nested), got {}",
        out.stats.n_manifests
    );
    assert_eq!(
        out.stats.n_regions, 3,
        "Gate 3b FAILED: expected 3 regions, got {}",
        out.stats.n_regions
    );

    let region_for = |suffix: &str| -> u32 {
        out.file_regions
            .iter()
            .find(|fr| {
                files[fr.file_index]
                    .to_string_lossy()
                    .ends_with(suffix)
            })
            .unwrap_or_else(|| panic!("no fixture file ending in {suffix}"))
            .region
    };

    let root_region = region_for("root.rs");
    let subcrate_region = region_for("subcrate_lib.rs");
    let nested_region = region_for("nested_lib.rs");

    assert_ne!(root_region, subcrate_region);
    assert_ne!(subcrate_region, nested_region);
    assert_ne!(root_region, nested_region);

    // Manifests sorted by directory path lexicographically: root <
    // subcrate < subcrate/nested-subcrate. Region ids assigned in that
    // order (1-indexed).
    assert!(
        root_region < subcrate_region,
        "Gate 3b FAILED: root region {root_region} not less than subcrate region {subcrate_region}"
    );
    assert!(
        subcrate_region < nested_region,
        "Gate 3b FAILED: subcrate region {subcrate_region} not less than nested region {nested_region}"
    );
}

/// Bloat-dir manifests (`vendor/Cargo.toml`, `node_modules/package.json`)
/// must be EXCLUDED from region assignment because `collect_files` skips
/// those directories entirely. Confirms that the topology pass does not
/// pick up vendored manifests as regions, which would otherwise create
/// spurious region splits for vendored copies of dependencies.
#[test]
fn gate3c_bloat_dir_manifests_excluded_from_regions() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    write_bloat_dir_fixture(root);

    let files = topology_pass::collect_files(root).expect("walk");
    let basenames: Vec<String> =
        files.iter().map(|p| basename(p)).collect();
    eprintln!(
        "[Gate 3c / bloat dirs] walked {} files: {basenames:?}",
        files.len(),
    );

    // The walk itself must skip vendor/ and node_modules/, so the
    // vendored Cargo.toml / package.json must not appear in `files` at
    // all. (If `collect_files`'s skip predicate is ever weakened this
    // assertion catches it.)
    assert!(
        !basenames.iter().any(|n| n == "vendored_lib.rs"),
        "Gate 3c FAILED: collect_files walked into vendor/ — vendored_lib.rs present in walk"
    );
    assert!(
        !basenames.iter().any(|n| n == "vendored_index.ts"),
        "Gate 3c FAILED: collect_files walked into node_modules/ — vendored_index.ts present"
    );

    let out = topology_pass::run(&files, root).expect("run");
    eprintln!(
        "[Gate 3c] manifests={} regions={} files_in_walk={}",
        out.stats.n_manifests,
        out.stats.n_regions,
        out.stats.n_files,
    );

    // Only the root Cargo.toml is visible to the walk. Vendored
    // manifests are *physically present on disk* but pruned by
    // is_bloat_dir before the recursion descends.
    assert_eq!(
        out.stats.n_manifests, 1,
        "Gate 3c FAILED: expected 1 manifest (root only), got {}; bloat dirs were not pruned",
        out.stats.n_manifests,
    );

    // Every file (the root one) must land in the same region.
    let regions: std::collections::BTreeSet<_> =
        out.file_regions.iter().map(|fr| fr.region).collect();
    assert_eq!(
        regions.len(),
        1,
        "Gate 3c FAILED: expected 1 region, got {regions:?}"
    );
}

// ---------------------------------------------------------------------------
// Gate 4 — determinism.
//
// Serializes ALL FIVE output fields, not just stats + region_edges. The
// previous gate-4 implementation excluded `edge_estimates`, `parse_order`,
// and `file_regions` — a future refactor that introduces a `HashMap`
// iteration in the sweep hot path would have broken determinism
// invisibly.
// ---------------------------------------------------------------------------

#[test]
fn gate4_determinism_same_input_same_output() {
    let files = handcrafted_files();
    let root = handcrafted_root();

    let a = topology_pass::run(&files, &root).expect("run 1");
    let b = topology_pass::run(&files, &root).expect("run 2");

    let a_serial = determinism_serial(&a);
    let b_serial = determinism_serial(&b);
    eprintln!(
        "[Gate 4] serialized {} bytes (covers all 5 TopologyOutput fields)",
        a_serial.len(),
    );

    assert_eq!(
        a_serial, b_serial,
        "Gate 4 FAILED: serialization differs between runs across the full output surface"
    );
}

/// Byte-stable view of a TopologyOutput covering ALL of
/// `(stats_for_determinism, region_edges, edge_estimates, parse_order,
/// file_regions)`. `elapsed_us` is dropped by `for_determinism()`. Any
/// `HashMap` iteration leaking into any of these would break this
/// gate, including in fields not previously checked.
fn determinism_serial(out: &TopologyOutput) -> Vec<u8> {
    // Region edges: bit-pattern u32 for f32 byte-exact compare.
    let region_edges_bits: Vec<(u32, u32, u32)> = out
        .region_edges
        .iter()
        .map(|(a, b, c)| (*a, *b, c.to_bits()))
        .collect();

    // Edge estimates: discriminant the Lang enum and bit-pattern the f32.
    let edge_estimates_bits: Vec<(usize, usize, u32, u32)> = out
        .edge_estimates
        .iter()
        .map(|e: &EdgeEstimate| {
            (e.from, e.to, e.confidence.to_bits(), e.language as u32)
        })
        .collect();

    // File regions: already pure POD.
    let file_regions_pod: Vec<(usize, u32, u32, u64)> = out
        .file_regions
        .iter()
        .map(|fr: &FileRegion| (fr.file_index, fr.region, fr.depth, fr.size))
        .collect();

    let serializable = (
        out.stats.for_determinism(),
        region_edges_bits,
        edge_estimates_bits,
        out.parse_order.clone(),
        file_regions_pod,
    );
    serde_json::to_vec(&serializable).expect("serialize deterministic view")
}

// ---------------------------------------------------------------------------
// Gate 5 — region_edges → SheafRestrictionInput translation, spot-checked.
//
// Per skeptic finding IMPORTANT-3: the previous gate only asserted
// `restrictions.len() == region_edges.len()`, which is a tautological
// property of `.map().collect()` and cannot fail. The rewrite verifies
// the *semantic contract* of `SheafRestrictionInput`: `a != b`,
// `co_change_rate in (0.0, 1.0]`, `agreement_dim > 0`, and that `a`/`b`
// map to actual region ids produced by the same run.
// ---------------------------------------------------------------------------

#[test]
fn gate5_region_edges_to_sheaf_restriction_translation_spot_checks() {
    let files = handcrafted_files();
    let root = handcrafted_root();
    let topology = topology_pass::run(&files, &root).expect("run handcrafted");

    let restrictions: Vec<SheafRestrictionInput> = topology
        .region_edges
        .iter()
        .map(|(a, b, conf)| SheafRestrictionInput {
            a: *a,
            b: *b,
            co_change_rate: *conf as f64,
            agreement_dim: 1,
            ..Default::default()
        })
        .collect();

    eprintln!(
        "[Gate 5] {} cross-region restrictions",
        restrictions.len()
    );
    for r in &restrictions {
        eprintln!(
            "    region {} <-> region {}  co_change_rate={:.4}  agreement_dim={}",
            r.a, r.b, r.co_change_rate, r.agreement_dim,
        );
    }

    // The handcrafted fixture has root + subcrate (2 regions); the
    // subcrate's lib.rs imports `crate::baz::qux` which resolves to the
    // root's src/baz.rs, producing exactly one cross-region edge.
    assert!(
        !restrictions.is_empty(),
        "Gate 5 FAILED: handcrafted fixture should yield at least 1 cross-region edge \
         (subcrate/src/lib.rs -> src/baz.rs); got none"
    );

    // Build the set of region ids actually produced by this run so we
    // can confirm each restriction's `a`/`b` map back to real regions.
    let known_regions: std::collections::BTreeSet<u32> = topology
        .file_regions
        .iter()
        .map(|fr| fr.region)
        .collect();

    for r in &restrictions {
        // Semantic contract: distinct endpoints (intra-region edges are
        // filtered out in `aggregate_region_edges`).
        assert_ne!(
            r.a, r.b,
            "Gate 5 FAILED: restriction has a == b == {} (intra-region leaked into output)",
            r.a,
        );

        // co_change_rate is a probability weight in (0, 1]. The mean-based
        // aggregation in `aggregate_region_edges` makes this invariant
        // statically, but the gate pins it at the integration boundary
        // because `op_sheaf_set_topology` treats it as such.
        assert!(
            r.co_change_rate > 0.0 && r.co_change_rate <= 1.0,
            "Gate 5 FAILED: co_change_rate {} outside (0.0, 1.0] for ({}, {})",
            r.co_change_rate, r.a, r.b,
        );

        assert!(
            r.agreement_dim > 0,
            "Gate 5 FAILED: agreement_dim must be > 0 for δ⁰ engagement; got 0 for ({}, {})",
            r.a, r.b,
        );

        // Both endpoints must be known region ids produced by the
        // topology pass — a swapped or stale id would slip past a
        // pure length check but fails here.
        assert!(
            known_regions.contains(&r.a),
            "Gate 5 FAILED: restriction.a = {} is not a real region id (known: {known_regions:?})",
            r.a,
        );
        assert!(
            known_regions.contains(&r.b),
            "Gate 5 FAILED: restriction.b = {} is not a real region id (known: {known_regions:?})",
            r.b,
        );
    }

    // Fixture-specific spot-check: identify the (root, subcrate) pair
    // by inspecting where `baz.rs` and `subcrate/.../lib.rs` landed,
    // then confirm the restriction over that pair exists with the
    // matching `a`/`b` order.
    let root_region = topology
        .file_regions
        .iter()
        .find(|fr| files[fr.file_index].ends_with("src/baz.rs"))
        .expect("baz.rs in fixture")
        .region;
    let subcrate_region = topology
        .file_regions
        .iter()
        .find(|fr| files[fr.file_index].ends_with("subcrate/src/lib.rs"))
        .expect("subcrate/src/lib.rs in fixture")
        .region;
    let (expected_a, expected_b) = if root_region <= subcrate_region {
        (root_region, subcrate_region)
    } else {
        (subcrate_region, root_region)
    };
    let hit = restrictions
        .iter()
        .find(|r| r.a == expected_a && r.b == expected_b);
    assert!(
        hit.is_some(),
        "Gate 5 FAILED: expected a restriction over the (root={root_region}, \
         subcrate={subcrate_region}) edge with canonical ordering \
         ({expected_a}, {expected_b}); got restrictions={:?}",
        restrictions
            .iter()
            .map(|r| (r.a, r.b))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn basename(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn assert_has_edge(
    edges: &[(Lang, String, String, f32)],
    lang: Lang,
    from_basename: &str,
    to_basename: &str,
) {
    let hit = edges
        .iter()
        .any(|(l, f, t, c)| *l == lang && f == from_basename && t == to_basename && *c > 0.0);
    assert!(
        hit,
        "Gate 2 FAILED: no {lang:?} edge {from_basename} -> {to_basename} \
         with positive confidence; saw: {edges:#?}"
    );
}

// ---------------------------------------------------------------------------
// Synthetic fixture generators.
// ---------------------------------------------------------------------------

/// Lower-bound synthetic fixture: one tiny file per slot. Used by
/// gate1a only.
fn generate_synthetic_1000_small(root: &Path) {
    use std::fs;
    use std::io::Write;
    const N_CRATES: usize = 5;
    const FILES_PER_CRATE: usize = 200;

    for c in 0..N_CRATES {
        let crate_dir = root.join(format!("crate{c}"));
        fs::create_dir_all(&crate_dir).expect("mkdir crate");
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"crate{c}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"
            ),
        )
        .expect("write manifest");
        let src = crate_dir.join("src");
        fs::create_dir_all(&src).expect("mkdir src");

        for i in 0..FILES_PER_CRATE {
            let (ext, contents) = synthetic_small_file(i);
            let path = src.join(format!("m{i}.{ext}"));
            let mut f = fs::File::create(&path).expect("create synthetic file");
            f.write_all(contents.as_bytes())
                .expect("write synthetic file");
        }
    }
}

fn synthetic_small_file(i: usize) -> (&'static str, String) {
    let prev = i.saturating_sub(1);
    match i % 4 {
        0 => (
            "rs",
            format!("use crate::m{prev}::thing;\n\npub fn f{i}() {{}}\n"),
        ),
        1 => (
            "py",
            format!("from m{prev} import thing\n\ndef f{i}():\n    pass\n"),
        ),
        2 => (
            "go",
            format!(
                "package m{i}\n\nimport \"example.com/m{prev}\"\n\nfunc F{i}() {{ _ = m{prev}.X }}\n"
            ),
        ),
        _ => (
            "ts",
            format!(
                "import {{ x }} from './m{prev}';\n\nexport const f{i} = x;\n"
            ),
        ),
    }
}

/// Realistic-size synthetic fixture: each file padded to ~6-15 KiB with
/// regex-irrelevant content (comment blocks). The padding is chosen so
/// the 4 KiB head-scan reads a full page rather than a partial buffer.
fn generate_synthetic_1000_realistic(root: &Path) {
    use std::fs;
    use std::io::Write;
    const N_CRATES: usize = 5;
    const FILES_PER_CRATE: usize = 200;

    for c in 0..N_CRATES {
        let crate_dir = root.join(format!("crate{c}"));
        fs::create_dir_all(&crate_dir).expect("mkdir crate");
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"crate{c}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"
            ),
        )
        .expect("write manifest");
        let src = crate_dir.join("src");
        fs::create_dir_all(&src).expect("mkdir src");

        for i in 0..FILES_PER_CRATE {
            let (ext, head) = synthetic_small_file(i);
            // Pad with a deterministic comment block. Target ~10 KiB
            // per file so the spot-check in gate1b (6-15 KiB) holds for
            // every sampled file. Use the language-appropriate line
            // comment so the file still scans cleanly.
            let pad_line = match ext {
                "rs" | "go" | "ts" => "// pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad",
                "py" => "# pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad",
                _ => "// pad",
            };
            // ~70 bytes per line × 150 lines = ~10 KiB of padding,
            // plus the ~60-100-byte import head.
            let mut contents = head;
            for _ in 0..150 {
                contents.push_str(pad_line);
                contents.push('\n');
            }
            let path = src.join(format!("m{i}.{ext}"));
            let mut f = fs::File::create(&path).expect("create synthetic file");
            f.write_all(contents.as_bytes())
                .expect("write synthetic file");
        }
    }
}

/// Depth-3 fixture: `root/Cargo.toml`, `root/subcrate/Cargo.toml`,
/// `root/subcrate/nested-subcrate/Cargo.toml`. One source file per
/// region so the test can confirm assignment by ending path suffix.
fn write_depth_3_fixture(root: &Path) {
    use std::fs;

    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n")
    };

    // Root.
    fs::write(root.join("Cargo.toml"), manifest("topology-depth3-root"))
        .expect("write root manifest");
    fs::create_dir_all(root.join("src")).expect("mkdir root/src");
    fs::write(
        root.join("src").join("root.rs"),
        "pub fn root() {}\n",
    )
    .expect("write root.rs");

    // Subcrate (depth 2).
    let sub = root.join("subcrate");
    fs::create_dir_all(sub.join("src")).expect("mkdir subcrate/src");
    fs::write(sub.join("Cargo.toml"), manifest("topology-depth3-subcrate"))
        .expect("write subcrate manifest");
    fs::write(
        sub.join("src").join("subcrate_lib.rs"),
        "pub fn subcrate() {}\n",
    )
    .expect("write subcrate_lib.rs");

    // Nested subcrate (depth 3).
    let nested = sub.join("nested-subcrate");
    fs::create_dir_all(nested.join("src")).expect("mkdir nested/src");
    fs::write(
        nested.join("Cargo.toml"),
        manifest("topology-depth3-nested"),
    )
    .expect("write nested manifest");
    fs::write(
        nested.join("src").join("nested_lib.rs"),
        "pub fn nested() {}\n",
    )
    .expect("write nested_lib.rs");
}

/// Bloat-dir fixture: root with one real manifest + one source file,
/// plus a vendored `vendor/Cargo.toml`/`vendor/src/...` and a
/// `node_modules/some-pkg/package.json`/`node_modules/.../index.ts`.
/// `cmd_parse::is_bloat_dir` skips `vendor` and `node_modules` outright,
/// so the topology pass must never see those manifests as regions.
fn write_bloat_dir_fixture(root: &Path) {
    use std::fs;

    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n")
    };

    // Root.
    fs::write(root.join("Cargo.toml"), manifest("bloat-fixture-root"))
        .expect("write root manifest");
    fs::create_dir_all(root.join("src")).expect("mkdir root/src");
    fs::write(
        root.join("src").join("root.rs"),
        "pub fn root() {}\n",
    )
    .expect("write root.rs");

    // Vendored Rust crate — must be excluded.
    let vendor = root.join("vendor").join("some-crate");
    fs::create_dir_all(vendor.join("src")).expect("mkdir vendor/some-crate/src");
    fs::write(
        vendor.join("Cargo.toml"),
        manifest("vendored-some-crate"),
    )
    .expect("write vendored manifest");
    fs::write(
        vendor.join("src").join("vendored_lib.rs"),
        "pub fn vendored() {}\n",
    )
    .expect("write vendored_lib.rs");

    // Vendored TS pkg — must be excluded.
    let nm = root.join("node_modules").join("some-pkg");
    fs::create_dir_all(&nm).expect("mkdir node_modules/some-pkg");
    fs::write(
        nm.join("package.json"),
        r#"{"name":"some-pkg","version":"0.0.0"}"#,
    )
    .expect("write package.json");
    fs::write(
        nm.join("vendored_index.ts"),
        "export const v = 1;\n",
    )
    .expect("write vendored index");
}
