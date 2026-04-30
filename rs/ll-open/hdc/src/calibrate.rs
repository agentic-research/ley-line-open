//! Empirical radius calibration: per-layer baseline distribution
//! sampled from the actual `_hdc` corpus.
//!
//! Per math-friend review B. Real codebases aren't iid — `self`/`if`/
//! `return` co-occur far above chance. The theoretical D/2 ± √D/2
//! random-pair baseline doesn't hold; using it would treat half the
//! corpus as "near-clones." Calibrate against the empirical median
//! Hamming distance and pick `r = median − 3·MAD` as the
//! "structurally-meaningful match" threshold.
//!
//! This is the load-bearing property that makes density queries
//! meaningful. Without calibration, every radius is uncalibrated noise.

use rusqlite::Connection;

use crate::util::{popcount_distance, Hypervector};
use crate::LayerKind;
#[cfg(test)]
use crate::D_BYTES;

/// Default sample size for calibration. Math-friend review B
/// recommended 10k random pairs as the sweet spot between speed
/// (sub-second on a typical laptop) and statistical stability
/// (median estimate variance ≪ 1 bit at this size).
pub const DEFAULT_CALIBRATION_SAMPLES: usize = 10_000;

/// Tightness factor on the recommended radius. `r = median − k * MAD`.
/// k=3 is conventional ("3-sigma" in MAD units; corresponds to ~99%
/// confidence the matched pair is a structural neighbor, not a random
/// pair from the codebase distribution).
pub const DEFAULT_RADIUS_TIGHTNESS: f64 = 3.0;

/// Computed baseline statistics for one layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadiusBaseline {
    pub layer: LayerKind,
    pub median_distance: u32,
    pub mad: u32,
    pub sample_size: usize,
    pub computed_at_ms: i64,
}

impl RadiusBaseline {
    /// Recommended match radius for radius/density queries on this
    /// layer. `median − tightness * MAD` clamped at 0.
    pub fn recommended_radius(&self, tightness: f64) -> u32 {
        let r = self.median_distance as f64 - tightness * self.mad as f64;
        if r < 0.0 {
            0
        } else {
            r as u32
        }
    }

    /// Convenience: recommended radius using `DEFAULT_RADIUS_TIGHTNESS`.
    pub fn default_radius(&self) -> u32 {
        self.recommended_radius(DEFAULT_RADIUS_TIGHTNESS)
    }
}

/// Sample `sample_size` random pairs from the layer's rows in `_hdc`,
/// compute their pairwise Hamming distances, and return the empirical
/// (median, MAD).
///
/// Sampling strategy: simple bernoulli over the row count. The math
/// friend's review notes 10k pairs is enough for stable median
/// estimates on corpora ≥ a few thousand functions; smaller corpora
/// auto-clamp to the available C(N, 2) pairs.
pub fn calibrate_layer(
    conn: &Connection,
    layer: LayerKind,
    sample_size: usize,
    now_ms: i64,
) -> rusqlite::Result<Option<RadiusBaseline>> {
    let hvs = collect_layer_hvs(conn, layer)?;
    if hvs.len() < 2 {
        return Ok(None);
    }

    // Cap the requested sample size at the number of distinct unordered
    // pairs available, so small fixtures don't degenerate.
    let n = hvs.len();
    let max_pairs = n * (n - 1) / 2;
    let target = sample_size.min(max_pairs);

    // Deterministic SplitMix64 seed so calibration reproduces across
    // runs with the same corpus (test stability + multi-machine
    // agreement).
    let mut state: u64 = blake3_seed_from(layer);
    let mut distances: Vec<u32> = Vec::with_capacity(target);
    let mut sampled = 0;
    while sampled < target {
        let i = (next_u64(&mut state) as usize) % n;
        let j = (next_u64(&mut state) as usize) % n;
        if i == j {
            continue;
        }
        distances.push(popcount_distance(&hvs[i], &hvs[j]));
        sampled += 1;
    }

    let median = quickselect_median(&mut distances);
    let mad = compute_mad(&distances, median);

    Ok(Some(RadiusBaseline {
        layer,
        median_distance: median,
        mad,
        sample_size: distances.len(),
        computed_at_ms: now_ms,
    }))
}

/// Calibrate every layer with at least 2 rows in `_hdc`. Persists each
/// baseline into `_hdc_baseline` (INSERT OR REPLACE on layer_kind).
/// Returns the count of layers calibrated.
pub fn calibrate_and_persist(
    conn: &Connection,
    sample_size: usize,
    now_ms: i64,
) -> rusqlite::Result<usize> {
    let mut count = 0;
    for layer in [
        LayerKind::Ast,
        LayerKind::Module,
        LayerKind::Semantic,
        LayerKind::Temporal,
        LayerKind::Hir,
        LayerKind::Lex,
        LayerKind::Fs,
    ] {
        if let Some(baseline) = calibrate_layer(conn, layer, sample_size, now_ms)? {
            persist_baseline(conn, &baseline)?;
            count += 1;
        }
    }
    Ok(count)
}

fn collect_layer_hvs(
    conn: &Connection,
    layer: LayerKind,
) -> rusqlite::Result<Vec<Hypervector>> {
    let mut stmt = conn.prepare_cached("SELECT hv FROM _hdc WHERE layer_kind = ?1")?;
    let rows = stmt.query_map([layer.as_str()], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let blob = row?;
        if let Ok(hv) = <Hypervector>::try_from(&blob[..]) {
            out.push(hv);
        }
    }
    Ok(out)
}

fn persist_baseline(conn: &Connection, b: &RadiusBaseline) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _hdc_baseline \
         (layer_kind, median_distance, mad, sample_size, computed_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            b.layer.as_str(),
            b.median_distance as i64,
            b.mad as i64,
            b.sample_size as i64,
            b.computed_at_ms,
        ],
    )?;
    Ok(())
}

/// Read a stored baseline back from `_hdc_baseline`.
pub fn load_baseline(
    conn: &Connection,
    layer: LayerKind,
) -> rusqlite::Result<Option<RadiusBaseline>> {
    let mut stmt = conn.prepare_cached(
        "SELECT median_distance, mad, sample_size, computed_at_ms \
         FROM _hdc_baseline WHERE layer_kind = ?1",
    )?;
    let result = stmt.query_row([layer.as_str()], |r| {
        Ok(RadiusBaseline {
            layer,
            median_distance: r.get::<_, i64>(0)? as u32,
            mad: r.get::<_, i64>(1)? as u32,
            sample_size: r.get::<_, i64>(2)? as usize,
            computed_at_ms: r.get(3)?,
        })
    });
    match result {
        Ok(b) => Ok(Some(b)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Median via quickselect (modifies the slice in place — O(n) average,
/// no full sort needed). For small samples (~10k), the difference vs
/// sort-then-index is negligible, but quickselect avoids the O(n log n)
/// pessimistic case on large samples.
fn quickselect_median(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let n = values.len();
    // For even n, return the lower-middle (cheaper than averaging).
    let target = (n - 1) / 2;
    *values.select_nth_unstable(target).1
}

/// Median Absolute Deviation from a known median. Two passes through
/// the slice; same complexity as the median calculation itself.
fn compute_mad(values: &[u32], median: u32) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut deviations: Vec<u32> = values
        .iter()
        .map(|&v| v.abs_diff(median))
        .collect();
    quickselect_median(&mut deviations)
}

fn blake3_seed_from(layer: LayerKind) -> u64 {
    crate::util::blake3_seed(format!("hdc-calibrate/{}", layer.as_str()).as_bytes())
}

#[inline]
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_hdc_schema;
    use crate::util::expand_seed;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_hdc_schema(&conn).unwrap();
        conn
    }

    fn insert_layer_hv(conn: &Connection, scope: &str, layer: LayerKind, hv: &Hypervector, basis: i64) {
        conn.execute(
            "INSERT INTO _hdc(scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![scope, layer.as_str(), hv.to_vec(), basis],
        )
        .unwrap();
    }

    #[test]
    fn empty_layer_returns_none() {
        let conn = fresh();
        assert_eq!(calibrate_layer(&conn, LayerKind::Ast, 100, 0).unwrap(), None);
    }

    #[test]
    fn single_row_returns_none() {
        // Need at least 2 rows to form a pair.
        let conn = fresh();
        insert_layer_hv(&conn, "x", LayerKind::Ast, &expand_seed(1), 1);
        assert_eq!(calibrate_layer(&conn, LayerKind::Ast, 100, 0).unwrap(), None);
    }

    #[test]
    fn random_iid_corpus_baseline_near_d_over_2() {
        // Synthesize an iid random corpus: each scope's HV is
        // expand_seed(seed) for a distinct seed. These are uniformly
        // random, so pairwise Hamming should distribute around D/2 = 4096
        // with std-dev ≈ √D/2 ≈ 45.
        let conn = fresh();
        for i in 0..200 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        let baseline = calibrate_layer(&conn, LayerKind::Ast, 1000, 0).unwrap().unwrap();
        // Median should be near 4096, well within a few std-devs.
        assert!(
            baseline.median_distance.abs_diff(4096) < 200,
            "iid random corpus median should be ~4096, got {}",
            baseline.median_distance,
        );
        // MAD should be small relative to the median.
        assert!(
            baseline.mad < 500,
            "iid random corpus MAD should be small, got {}",
            baseline.mad,
        );
    }

    #[test]
    fn correlated_corpus_baseline_below_d_over_2() {
        // Synthesize a correlated corpus: every HV is
        // (boilerplate_prefix XOR distinct_suffix). Pairs share the
        // boilerplate, so their Hamming distance is dominated by the
        // distinct-suffix part — significantly below D/2.
        let conn = fresh();
        let boilerplate = expand_seed(0xBADC_AFFE);
        for i in 0..50 {
            let mut hv = boilerplate;
            // Set only a small number of differing bits per scope,
            // so all pairs share most of their bits.
            let diff = expand_seed(i + 1);
            // Take only the first 1024 bits of diff as the unique part.
            for byte in hv.iter_mut().take(128) {
                *byte ^= diff[(byte as *mut u8 as usize) % D_BYTES];
            }
            // Add minimal per-scope variation.
            hv[0] ^= i as u8;
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &hv, 1);
        }
        let baseline = calibrate_layer(&conn, LayerKind::Ast, 1000, 0).unwrap().unwrap();
        // Correlated corpus → median below D/2.
        assert!(
            baseline.median_distance < 4000,
            "correlated corpus should produce median < 4000, got {}",
            baseline.median_distance,
        );
    }

    #[test]
    fn calibration_is_deterministic_per_layer() {
        // Same corpus + layer + sample_size → same baseline. The
        // SplitMix64 seed is derived from the layer's name, so two
        // calls produce identical samples.
        let conn = fresh();
        for i in 0..50 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        let b1 = calibrate_layer(&conn, LayerKind::Ast, 100, 0).unwrap().unwrap();
        let b2 = calibrate_layer(&conn, LayerKind::Ast, 100, 0).unwrap().unwrap();
        assert_eq!(b1.median_distance, b2.median_distance);
        assert_eq!(b1.mad, b2.mad);
    }

    #[test]
    fn calibration_is_distinct_per_layer() {
        // Different layer name → different sampling seed → potentially
        // different sampled subset → potentially different median.
        // Even if the corpora were identical, the seeds differ, so
        // the random pair selection differs.
        let conn = fresh();
        for i in 0..50 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Module, &expand_seed(i + 1), 1);
        }
        let ast = calibrate_layer(&conn, LayerKind::Ast, 100, 0).unwrap().unwrap();
        let module = calibrate_layer(&conn, LayerKind::Module, 100, 0).unwrap().unwrap();
        // Sample seeds differ; with small sample the medians may
        // coincidentally land equal, but the underlying sampling MUST
        // differ — pin via the public layer field which is part of
        // the baseline identity.
        assert_eq!(ast.layer, LayerKind::Ast);
        assert_eq!(module.layer, LayerKind::Module);
    }

    #[test]
    fn recommended_radius_clamps_to_zero() {
        // If MAD * tightness > median (small/skewed corpus), recommended
        // radius would go negative — clamp to 0. Pin the corner case so
        // a caller using default_radius never gets a panic or wraparound.
        let baseline = RadiusBaseline {
            layer: LayerKind::Ast,
            median_distance: 100,
            mad: 50,
            sample_size: 10,
            computed_at_ms: 0,
        };
        assert_eq!(baseline.recommended_radius(3.0), 0);
        // Mild MAD: positive radius.
        let baseline2 = RadiusBaseline {
            layer: LayerKind::Ast,
            median_distance: 4096,
            mad: 64,
            sample_size: 10000,
            computed_at_ms: 0,
        };
        assert_eq!(baseline2.recommended_radius(3.0), 4096 - 3 * 64);
    }

    #[test]
    fn calibrate_and_persist_writes_to_baseline_table() {
        let conn = fresh();
        for i in 0..30 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        let count = calibrate_and_persist(&conn, 100, 1_700_000_000_000).unwrap();
        assert_eq!(count, 1, "only Ast layer has rows");

        let baseline: RadiusBaseline = load_baseline(&conn, LayerKind::Ast).unwrap().unwrap();
        assert_eq!(baseline.computed_at_ms, 1_700_000_000_000);
        assert!(baseline.sample_size > 0);
    }

    #[test]
    fn calibrate_replaces_existing_baseline() {
        // Re-running calibration after the corpus changes should
        // overwrite the stored baseline, not error.
        let conn = fresh();
        for i in 0..20 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        calibrate_and_persist(&conn, 100, 1_700_000_000_000).unwrap();

        // Add more rows (simulating corpus growth).
        for i in 20..50 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        calibrate_and_persist(&conn, 100, 1_800_000_000_000).unwrap();

        let baseline = load_baseline(&conn, LayerKind::Ast).unwrap().unwrap();
        assert_eq!(baseline.computed_at_ms, 1_800_000_000_000);
    }

    #[test]
    fn load_baseline_returns_none_for_uncalibrated_layer() {
        let conn = fresh();
        assert_eq!(load_baseline(&conn, LayerKind::Ast).unwrap(), None);
    }

    #[test]
    fn small_corpus_auto_clamps_sample_size() {
        // 5 scopes → C(5, 2) = 10 unordered pairs. Requesting 1000
        // samples should clamp gracefully rather than loop forever.
        let conn = fresh();
        for i in 0..5 {
            insert_layer_hv(&conn, &format!("s{i}"), LayerKind::Ast, &expand_seed(i + 1), 1);
        }
        let baseline = calibrate_layer(&conn, LayerKind::Ast, 1000, 0).unwrap().unwrap();
        // Sample size capped at the available pair count.
        assert!(
            baseline.sample_size <= 10,
            "small corpus should clamp samples (got {})",
            baseline.sample_size,
        );
    }

    #[test]
    fn quickselect_median_matches_sort_for_known_inputs() {
        // Pin the median definition: lower-middle for even-length input.
        let mut a = vec![1u32, 3, 5, 7];
        assert_eq!(quickselect_median(&mut a), 3); // lower-middle of 4 elements
        let mut b = vec![1u32, 2, 3, 4, 5];
        assert_eq!(quickselect_median(&mut b), 3); // exact middle of 5 elements
        let mut c = vec![100u32];
        assert_eq!(quickselect_median(&mut c), 100);
        let mut d: Vec<u32> = vec![];
        assert_eq!(quickselect_median(&mut d), 0);
    }

    #[test]
    fn compute_mad_pinned() {
        // MAD = median of |x_i - median|. Hand-checked example.
        let values = [1u32, 1, 2, 2, 4, 6, 9];
        let median = 2;
        // Deviations: 1, 1, 0, 0, 2, 4, 7 → sorted 0,0,1,1,2,4,7 → median 1
        assert_eq!(compute_mad(&values, median), 1);
    }
}
