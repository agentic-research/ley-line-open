//! End-to-end gates for the topology pre-pass (bead `ley-line-open-9d3208`).
//!
//! Five falsifiability gates from the bead description:
//!
//! 1. **Cost ceiling** — 1000-file synthetic fixture; `run` returns in
//!    under 150 ms wall time.
//! 2. **Recall** — hand-crafted multi-language fixture; ≥4 of 4 imports
//!    detected as `EdgeEstimate` entries with `confidence > 0.0`.
//! 3. **Manifest detection** — nested fixture with root + `subcrate/`
//!    manifests; 2 distinct regions, subcrate files get the deeper id.
//! 4. **Determinism** — same input twice → byte-identical
//!    `(stats_deterministic, sorted_region_edges)` serialization.
//! 5. **Translation compiles** — `region_edges → SheafRestrictionInput`
//!    builds and produces a vec of equal length.
//!
//! Each gate is its own `#[test]` so a failure in one doesn't mask
//! the others.

use std::path::{Path, PathBuf};

use leyline_cli_lib::daemon::sheaf_ops::SheafRestrictionInput;
use leyline_cli_lib::topology_pass::{self, Lang, TopologyOutput};

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
// ---------------------------------------------------------------------------

#[test]
fn gate1_cost_ceiling_under_150ms_on_1000_files() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    generate_synthetic_1000(root);

    let files = topology_pass::collect_files(root).expect("walk synthetic");
    assert!(
        files.len() >= 1000,
        "synthetic fixture should be ≥1000 files, got {}",
        files.len()
    );

    // Two-run timing: first run primes the OS page cache (fair to the
    // ceiling — the bead's "<150ms" budget assumes warm I/O).
    let _warm = topology_pass::run(&files, root).expect("warm-up run");

    let started = std::time::Instant::now();
    let out = topology_pass::run(&files, root).expect("timed run");
    let elapsed = started.elapsed();

    eprintln!(
        "[Gate 1] files={} elapsed={:?} regions={} edges={}",
        out.stats.n_files, elapsed, out.stats.n_regions, out.stats.n_edges
    );

    assert!(
        elapsed.as_millis() < 150,
        "Gate 1 FAILED: run took {:?} on {} files (budget: 150ms)",
        elapsed,
        files.len()
    );
}

// ---------------------------------------------------------------------------
// Gate 2 — recall on hand-crafted multi-language fixture.
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
    eprintln!("[Gate 2] {} edges detected:", edge_view.len());
    for (lang, f, t, c) in &edge_view {
        eprintln!("    {lang:?}  {f} -> {t}  (conf={c:.3})");
    }

    let langs_seen: std::collections::BTreeSet<Lang> =
        out.edge_estimates.iter().map(|e| e.language).collect();
    let required = [Lang::Go, Lang::Rust, Lang::Python, Lang::Ts];
    let missing: Vec<&Lang> = required
        .iter()
        .filter(|l| !langs_seen.contains(*l))
        .collect();
    assert!(
        missing.is_empty(),
        "Gate 2 FAILED: missing languages {missing:?}; saw {langs_seen:?}",
    );

    // Every detected edge must carry positive confidence.
    for e in &out.edge_estimates {
        assert!(
            e.confidence > 0.0,
            "Gate 2 FAILED: edge {e:?} has non-positive confidence"
        );
    }

    // Spot-check: each of the 4 specific imports we hand-crafted should
    // resolve to its intended target by basename.
    assert_has_edge(&edge_view, Lang::Go, "main.go", "bar.go");
    assert_has_edge(&edge_view, Lang::Rust, "lib.rs", "baz.rs");
    assert_has_edge(&edge_view, Lang::Python, "main.py", "bar.py");
    assert_has_edge(&edge_view, Lang::Ts, "main.ts", "util.ts");
}

// ---------------------------------------------------------------------------
// Gate 3 — manifest detection: 2 distinct regions, subcrate gets the
// deeper id.
// ---------------------------------------------------------------------------

#[test]
fn gate3_manifest_detection_nested_root_and_subcrate() {
    let files = handcrafted_files();
    let root = handcrafted_root();
    let out = topology_pass::run(&files, &root).expect("run handcrafted");

    let regions: std::collections::BTreeSet<_> =
        out.file_regions.iter().map(|fr| fr.region).collect();
    eprintln!(
        "[Gate 3] manifests={} regions={:?} n_regions_stat={}",
        out.stats.n_manifests, regions, out.stats.n_regions
    );
    assert_eq!(
        out.stats.n_manifests, 2,
        "Gate 3 FAILED: expected 2 manifests, got {}",
        out.stats.n_manifests
    );
    assert_eq!(
        regions.len(),
        2,
        "Gate 3 FAILED: expected exactly 2 distinct regions, got {regions:?}"
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
        "Gate 3 FAILED: subcrate and root share region id {subcrate_region}"
    );

    // Every file under subcrate/ MUST be in the subcrate region.
    for fr in &out.file_regions {
        let rel = files[fr.file_index].strip_prefix(&root).unwrap();
        if rel.starts_with("subcrate") {
            assert_eq!(
                fr.region, subcrate_region,
                "Gate 3 FAILED: subcrate file {} got region {} (expected {})",
                rel.display(),
                fr.region,
                subcrate_region
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Gate 4 — determinism.
// ---------------------------------------------------------------------------

#[test]
fn gate4_determinism_same_input_same_output() {
    let files = handcrafted_files();
    let root = handcrafted_root();

    let a = topology_pass::run(&files, &root).expect("run 1");
    let b = topology_pass::run(&files, &root).expect("run 2");

    let a_serial = determinism_serial(&a);
    let b_serial = determinism_serial(&b);
    eprintln!("[Gate 4] serial len={} bytes", a_serial.len());

    assert_eq!(
        a_serial, b_serial,
        "Gate 4 FAILED: serialization differs between runs"
    );
}

/// Byte-stable view of a TopologyOutput, excluding `elapsed_us`.
fn determinism_serial(out: &TopologyOutput) -> Vec<u8> {
    // (stats_deterministic, sorted_region_edges)
    let mut sorted_edges: Vec<(u32, u32, f32)> = out.region_edges.clone();
    sorted_edges.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let serializable = (
        out.stats.for_determinism(),
        // Convert f32 to bit-pattern u32 for byte-exact compare.
        sorted_edges
            .into_iter()
            .map(|(a, b, c)| (a, b, c.to_bits()))
            .collect::<Vec<_>>(),
    );
    serde_json::to_vec(&serializable).expect("serialize deterministic view")
}

// ---------------------------------------------------------------------------
// Gate 5 — translation compiles, vec length matches.
// ---------------------------------------------------------------------------

#[test]
fn gate5_region_edges_to_sheaf_restriction_input_translation() {
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
        "[Gate 5] region_edges.len()={} restrictions.len()={}",
        topology.region_edges.len(),
        restrictions.len()
    );
    assert_eq!(
        restrictions.len(),
        topology.region_edges.len(),
        "Gate 5 FAILED: translated vec length {} != region_edges length {}",
        restrictions.len(),
        topology.region_edges.len()
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

/// Generate a 1000-file synthetic fixture under `root`. Layout:
/// - 5 top-level manifests (Cargo.toml in each of 5 crate dirs)
/// - Each crate dir holds 200 files split across Rust / Python / Go / TS
/// - Each file has at most one import statement so the sweep has work
///   to do but doesn't run away.
fn generate_synthetic_1000(root: &Path) {
    use std::fs;
    use std::io::Write;
    const N_CRATES: usize = 5;
    const FILES_PER_CRATE: usize = 200;

    for c in 0..N_CRATES {
        let crate_dir = root.join(format!("crate{c}"));
        fs::create_dir_all(&crate_dir).expect("mkdir crate");
        // Manifest (anchors a region).
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
            let lang_pick = i % 4;
            let (ext, contents) = match lang_pick {
                0 => (
                    "rs",
                    format!(
                        "use crate::m{prev}::thing;\n\npub fn f{i}() {{}}\n",
                        prev = i.saturating_sub(1)
                    ),
                ),
                1 => (
                    "py",
                    format!("from m{prev} import thing\n\ndef f{i}():\n    pass\n", prev = i.saturating_sub(1)),
                ),
                2 => (
                    "go",
                    format!(
                        "package m{i}\n\nimport \"example.com/m{prev}\"\n\nfunc F{i}() {{ _ = m{prev}.X }}\n",
                        prev = i.saturating_sub(1)
                    ),
                ),
                _ => (
                    "ts",
                    format!(
                        "import {{ x }} from './m{prev}';\n\nexport const f{i} = x;\n",
                        prev = i.saturating_sub(1)
                    ),
                ),
            };
            let path = src.join(format!("m{i}.{ext}"));
            let mut f = fs::File::create(&path).expect("create synthetic file");
            f.write_all(contents.as_bytes())
                .expect("write synthetic file");
        }
    }
}
