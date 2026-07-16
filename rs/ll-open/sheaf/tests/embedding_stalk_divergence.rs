//! Rung 1 of ADR-0030 — the kill-switch experiment for sheaf-over-embeddings
//! (bead `ley-line-open-d4e605`).
//!
//! # The claim under test (binary)
//!
//! With a LOCALITY-PRESERVING stalk (an HDC hypervector built from the region
//! bytes) instead of the current cryptographic SHA-256 stalk, the sheaf's δ⁰
//! decision DIVERGES from a hash-equality decision on a *cosmetic* edit: there
//! exists a region edit where
//!
//! ```text
//!   δ⁰(HV_before, HV_after) < EPS     ("close, skip")
//!   SHA256(before) != SHA256(after)   ("changed, invalidate")
//! ```
//!
//! One constructed divergence proves the δ⁰ math stops being degenerate. If no
//! such divergence can be constructed even with locality-preserving stalks, the
//! thesis is dead at rung 1 and the honest reverse-dep BFS (`716c69`) ships.
//!
//! # Result (measured, this file)
//!
//! * **Rung 1 — divergence CONSTRUCTED (yes).** A whitespace-reflow edit
//!   (`accumulate`, derived facts identical) lands at `d/D = 0.039` under the
//!   locality-preserving stalk — comfortably below the pre-committed EPS of
//!   `0.10` (cosine ≈ 0.95) — while its SHA-256 stalk differs completely. The
//!   sub-EPS continuum that a cryptographic avalanche hash cannot express is
//!   real: the δ⁰ math stops being degenerate.
//!
//! * **Rung 2 — smoke test RED (flagged loudly, not asserted away).** Surface
//!   byte-trigram distance does NOT track derived-fact stability. A pure local
//!   rename (zero fact change) moves the stalk `d/D ≈ 0.18` — FARTHER than a
//!   fact-changing edit that swaps the callee and adds a CFG branch
//!   (`d/D ≈ 0.15`), and in a larger region the inversion is starker
//!   (rename `0.010` vs callee-swap `0.002`). The magnitude of surface change
//!   is driven by how many trigrams an edit touches, which is uncorrelated with
//!   whether the derived facts changed. This is the exact failure mode ADR-0030
//!   flags for rung 2, previewed here on the byte-level stalk. The
//!   rename-invariant AST-structural stalk (needs a parser, out of scope here)
//!   is the representation that could survive rung 2.
//!
//! # Why the δ⁰ computation here is faithful to the live sheaf
//!
//! The live δ⁰ per-edge quantity is [`CellComplex::edge_violation_squared`] =
//! `‖f_v·x_v − f_u·x_u‖²`. On every OSS-live input the restriction maps are
//! [`RestrictionMap::project_dim_range`] — an axis-aligned coordinate mask `P`
//! identical on both endpoints (necessity audit `716c69` / SESSION 2 log). So
//! the live per-edge δ⁰ is `‖P·(x_v − x_u)‖²`.
//!
//! We model a single region's edit as a two-node complex: node `BEFORE` holds
//! the pre-edit stalk, node `AFTER` the post-edit stalk, joined by one edge
//! whose restriction map is the live `project_dim_range` mask. Then
//! `edge_violation_squared(BEFORE, AFTER)` is *exactly* the sheaf's δ⁰ distance
//! between the two region representations — the number the "close, skip" gate
//! would consult. [`edge_violation_squared_is_masked_hamming`] pins that this
//! equals the masked Hamming distance numerically, so the divergence is stated
//! in the sheaf's own quantity, not a proxy.
//!
//! # The stalk representations
//!
//! * **Cryptographic (today's live stalk):** SHA-256 of the region bytes. An
//!   avalanche function — one input-bit change flips ~half the output bits, so
//!   cosmetic and total-rewrite edits are indistinguishable (both ≈ maximally
//!   distant). No sub-EPS continuum exists. This is the degeneracy `716c69`
//!   proved.
//!
//! * **Locality-preserving (the proposed swap):** a D=8192-bit HDC hypervector
//!   built by the classic char-trigram shingle-bundle (Kanerva sequence
//!   encoding — the same construction the HDC encoder's `leaf_content_hv` uses
//!   for token leaves): slide a 3-byte window over the region bytes, map each
//!   trigram to a hypervector via the substrate's `bytes_to_hv`, and
//!   majority-bundle them. Two byte strings that share most trigrams produce
//!   hypervectors that are *close* in Hamming space — a genuine metric with a
//!   sub-EPS continuum.
//!
//! # The δ⁰ metric on hypervector stalks
//!
//! Embed each HV bit as a `{0.0, 1.0}` f32 coordinate. Under the identity /
//! axis-aligned mask, `‖P·(x_v − x_u)‖²` sums `(bit_v − bit_u)²` over the masked
//! coordinates = the count of differing bits = the **Hamming distance** over
//! those coordinates. That is precisely the crate's native `popcount_distance`,
//! and precisely what `edge_violation_squared` returns. We report the distance
//! normalized by the width (`d/D`), whose scale is fixed by the representation:
//! `d/D = 0` for identical regions, `d/D ≈ 0.5` for unrelated regions
//! (independent hypervectors are orthogonal → half their bits differ). Charikar
//! (2002): `d/D = θ/π`, so `d/D` and cosine similarity are interchangeable
//! framings of the same number.

use leyline_hdc::util::bytes_to_hv;
use leyline_hdc::{D_BITS, D_BYTES, Hypervector, popcount_distance};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// The concrete region + edits (real Rust function bodies)
// ---------------------------------------------------------------------------

/// Baseline region: a small, complete Rust function.
const BEFORE: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut total = 0;
    for value in items {
        total += compute_weight(value);
    }
    total
}
";

/// COSMETIC edit for rung 1 — a pure whitespace/indentation reflow (blank
/// lines added around the body). No token changes at all: signature, callee
/// (`compute_weight`), bindings, and control-flow graph are byte-for-byte
/// identical modulo layout. Derived facts (`node_defs` / `node_refs` / CFG) do
/// NOT change, so skipping re-derivation here is a CORRECT true-skip. A content
/// hash still sees "changed" because the bytes changed.
const AFTER_COSMETIC: &str = "\
fn accumulate(items: &[i64]) -> i64 {

    let mut total = 0;
    for value in items {
        total += compute_weight(value);
    }

    total
}
";

/// MEANINGFUL edit — changes derived facts. Swaps the callee
/// (`compute_weight` → `compute_penalty`, a new `node_ref`) and adds a guard
/// branch (`if *value < 0 { continue; }`, a new CFG edge). A semantic optimizer
/// MUST NOT skip this.
const AFTER_MEANINGFUL: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut total = 0;
    for value in items {
        if *value < 0 {
            continue;
        }
        total += compute_penalty(value);
    }
    total
}
";

/// COSMETIC rename — semantically meaning-preserving (renames the local
/// `total` → `running_sum`; derived facts unchanged), used only to expose the
/// rung-2 red flag: under a byte-surface stalk a rename moves the vector as far
/// as, or farther than, the fact-changing [`AFTER_MEANINGFUL`] edit.
const AFTER_RENAME: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut running_sum = 0;
    for value in items {
        running_sum += compute_weight(value);
    }
    running_sum
}
";

// A larger region, to show the inversion is not a small-sample artifact.
const BIG_BEFORE: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut total = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let weight = compute_weight(record);
        if weight > threshold {
            total += weight;
            count += 1;
            if weight > max_seen {
                max_seen = weight;
            }
        }
    }
    let average = if count > 0 { total / count as i64 } else { 0 };
    Summary { total, count, max_seen, average }
}
";
/// Cosmetic rename in the big region (`total`→`running`, `weight`→`w`).
const BIG_RENAME: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut running = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let w = compute_weight(record);
        if w > threshold {
            running += w;
            count += 1;
            if w > max_seen {
                max_seen = w;
            }
        }
    }
    let average = if count > 0 { running / count as i64 } else { 0 };
    Summary { total: running, count, max_seen, average }
}
";
/// Fact-changing edit in the big region (callee `compute_weight`→`compute_penalty`).
const BIG_MEANINGFUL: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut total = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let weight = compute_penalty(record);
        if weight > threshold {
            total += weight;
            count += 1;
            if weight > max_seen {
                max_seen = weight;
            }
        }
    }
    let average = if count > 0 { total / count as i64 } else { 0 };
    Summary { total, count, max_seen, average }
}
";

// ---------------------------------------------------------------------------
// Stalk constructions
// ---------------------------------------------------------------------------

/// Cryptographic stalk (today's live sheaf stalk): SHA-256 of the region bytes.
fn sha256_stalk(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Locality-preserving stalk: char-trigram shingle-bundle hypervector.
///
/// For each 3-byte window of the region bytes, derive a hypervector via the
/// substrate's `bytes_to_hv` (domain-separated with a `"rung1-tri/"` tag) and
/// majority-bundle them into one D-bit vector. This is the same shingle-bundle
/// the HDC encoder's `leaf_content_hv` applies to token leaves — the substrate's
/// documented locality-preserving text encoding.
fn hv_stalk(bytes: &[u8]) -> Hypervector {
    // Pad to at least one trigram (mirrors leaf_content_hv's short-content rule).
    let padded = if bytes.len() < 3 {
        let mut p = bytes.to_vec();
        p.resize(3, 0u8);
        p
    } else {
        bytes.to_vec()
    };

    let trigram_hvs: Vec<Hypervector> = padded
        .windows(3)
        .map(|w| {
            let mut tag = Vec::with_capacity(b"rung1-tri/".len() + 3);
            tag.extend_from_slice(b"rung1-tri/");
            tag.extend_from_slice(w);
            bytes_to_hv(&tag)
        })
        .collect();

    majority_bundle(&trigram_hvs)
}

/// Strict-majority bundle of N hypervectors: bit `i` is set iff more than half
/// the inputs have it set. Similarity-preserving (unlike XOR-bind). Ties (even
/// N, exact half) resolve to 0 — irrelevant to the divergence, which turns on
/// gross Hamming magnitude, not single-bit ties.
fn majority_bundle(inputs: &[Hypervector]) -> Hypervector {
    let mut out = [0u8; D_BYTES];
    if inputs.is_empty() {
        return out;
    }
    let half = inputs.len() as u32 / 2;
    for bit in 0..D_BITS {
        let byte_idx = bit / 8;
        let bit_off = bit % 8;
        let mut count: u32 = 0;
        for hv in inputs {
            count += ((hv[byte_idx] >> bit_off) & 1) as u32;
        }
        if count > half {
            out[byte_idx] |= 1 << bit_off;
        }
    }
    out
}

/// Embed a D-bit hypervector as a length-D `{0.0, 1.0}` f32 stalk vector so it
/// can be a `CellComplex` node stalk. Under the axis-aligned restriction mask,
/// `edge_violation_squared` on two such stalks computes the masked Hamming
/// distance (see module docs).
fn hv_to_stalk_f32(hv: &Hypervector) -> Vec<f32> {
    let mut out = Vec::with_capacity(D_BITS);
    for bit in 0..D_BITS {
        let byte_idx = bit / 8;
        let bit_off = bit % 8;
        out.push(((hv[byte_idx] >> bit_off) & 1) as f32);
    }
    out
}

/// Hamming distance over the FIRST `k` bits only — the ground truth the
/// axis-aligned `project_dim_range(D, k)` restriction should reproduce through
/// `edge_violation_squared`.
fn hamming_first_k(a: &Hypervector, b: &Hypervector, k: usize) -> u32 {
    let mut d = 0u32;
    for bit in 0..k {
        let byte_idx = bit / 8;
        let bit_off = bit % 8;
        let ba = (a[byte_idx] >> bit_off) & 1;
        let bb = (b[byte_idx] >> bit_off) & 1;
        d += (ba ^ bb) as u32;
    }
    d
}

/// Compute the sheaf's real δ⁰ distance between two hypervector stalks: build a
/// two-node complex (BEFORE/AFTER) with one edge carrying the live
/// `project_dim_range(D, agreement_dim)` axis-aligned mask on both endpoints,
/// and return `edge_violation_squared` — `‖P·(x_after − x_before)‖²`.
fn sheaf_delta0_sq(before: &Hypervector, after: &Hypervector, agreement_dim: usize) -> f32 {
    const BEFORE_ID: u32 = 0;
    const AFTER_ID: u32 = 1;
    const EDGE_ID: u32 = 1_000_000;

    let mut cx = CellComplex::new(D_BITS);
    cx.add_node(BEFORE_ID, hv_to_stalk_f32(before));
    cx.add_node(AFTER_ID, hv_to_stalk_f32(after));
    cx.add_edge(
        EDGE_ID,
        BEFORE_ID,
        AFTER_ID,
        agreement_dim,
        Some("region-edit".into()),
        RestrictionMap::project_dim_range(D_BITS, agreement_dim),
        RestrictionMap::project_dim_range(D_BITS, agreement_dim),
        false,
    );
    cx.edge_violation_squared(BEFORE_ID, AFTER_ID)
        .expect("edge present")
}

fn frac(a: &Hypervector, b: &Hypervector) -> f64 {
    popcount_distance(a, b) as f64 / D_BITS as f64
}

// ---------------------------------------------------------------------------
// EPS — chosen from the representation's scale, not reverse-engineered
// ---------------------------------------------------------------------------

/// Skip threshold as a fraction of the width (`d/D`).
///
/// Derivation, pre-committed from the representation's geometry alone (NOT from
/// the measured edit distances):
/// * `d/D = 0.5` is the orthogonality floor — two INDEPENDENT hypervectors
///   differ in half their bits. That is the representation's "these are
///   unrelated regions" scale.
/// * Charikar: `d/D = θ/π`, so `d/D = 0.10` ⟺ `cosine ≈ cos(0.1π) ≈ 0.95`.
///
/// We commit `EPS_FRAC = 0.10`: "skip only if the two boundary embeddings are
/// ≥ 0.95 cosine-similar" — one fifth of the way from identical to orthogonal.
/// A defensible "essentially the same content" bar, fixed before observing
/// where any edit lands.
const EPS_FRAC: f64 = 0.10;

/// Axis-aligned agreement width fed to `project_dim_range` for the faithful
/// `edge_violation_squared` call. A real coordinate mask (the live restriction
/// shape), sized to keep the dense restriction matrix small while sampling a
/// fair 1/16 of the hypervector's exchangeable bits.
const AGREEMENT_DIM: usize = D_BITS / 16; // 512

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Faithfulness pin: the sheaf's `edge_violation_squared` under the live
/// `project_dim_range` mask equals the masked Hamming distance. This ties the
/// divergence below to the sheaf's own δ⁰ quantity, not a stand-in.
#[test]
fn edge_violation_squared_is_masked_hamming() {
    let before = hv_stalk(BEFORE.as_bytes());
    let cosmetic = hv_stalk(AFTER_COSMETIC.as_bytes());

    let delta0_sq = sheaf_delta0_sq(&before, &cosmetic, AGREEMENT_DIM);
    let hamming = hamming_first_k(&before, &cosmetic, AGREEMENT_DIM);

    // {0,1} embedding: each differing masked bit contributes exactly 1.0² to
    // ‖P·(x_after − x_before)‖², so δ⁰² == masked Hamming exactly (f32-exact at
    // these integer magnitudes).
    assert_eq!(
        delta0_sq, hamming as f32,
        "edge_violation_squared ({delta0_sq}) must equal masked Hamming ({hamming})",
    );
}

/// THE KILL-SWITCH TEST — rung 1. Construct the divergence: a locality-
/// preserving δ⁰ distance on a cosmetic (whitespace-reflow) edit is below a
/// pre-committed EPS ("close, skip") while the SHA-256 stalks differ
/// ("changed, invalidate"). This is the answer: divergence CONSTRUCTED = yes.
#[test]
fn locality_preserving_stalk_diverges_from_hash_on_cosmetic_edit() {
    // --- Cryptographic stalks: MUST differ (avalanche) ---
    let sha_before = sha256_stalk(BEFORE.as_bytes());
    let sha_cosmetic = sha256_stalk(AFTER_COSMETIC.as_bytes());
    assert_ne!(
        sha_before, sha_cosmetic,
        "SHA-256 must see the cosmetic edit as changed (bytes did change)",
    );

    // --- Locality-preserving stalks ---
    let hv_before = hv_stalk(BEFORE.as_bytes());
    let hv_cosmetic = hv_stalk(AFTER_COSMETIC.as_bytes());

    // The sheaf's own δ⁰ distance (faithful edge_violation_squared) on the
    // cosmetic edit, plus full-width popcount for the normalized report.
    let cosmetic_delta0_sq = sheaf_delta0_sq(&hv_before, &hv_cosmetic, AGREEMENT_DIM);
    let cosmetic_frac = frac(&hv_before, &hv_cosmetic);

    // Independent-hypervector orthogonality floor, measured on the substrate
    // for THIS D so EPS is anchored to the real geometry, not a textbook 0.5.
    let orth_floor = independent_pair_frac();

    eprintln!("--- Rung-1 divergence (bead ley-line-open-d4e605) ---");
    eprintln!("cosmetic edit = whitespace reflow (derived facts unchanged)");
    eprintln!("SHA-256(before)   = {}", hex8(&sha_before));
    eprintln!(
        "SHA-256(cosmetic) = {}  (differ: {})",
        hex8(&sha_cosmetic),
        sha_before != sha_cosmetic,
    );
    eprintln!(
        "locality-preserving δ⁰²(masked {AGREEMENT_DIM}) = {cosmetic_delta0_sq}; \
         full d/D = {cosmetic_frac:.4}",
    );
    eprintln!("orthogonality floor (independent pair) d/D = {orth_floor:.4}");
    eprintln!("EPS_FRAC = {EPS_FRAC} (cosine ≈ 0.95)  ⇒  DIVERGENCE: hash=changed, δ⁰=skip");

    // --- THE DIVERGENCE ASSERTION ---
    // Hash says "changed" (asserted above); locality-preserving δ⁰ says
    // "close, skip" — cosmetic normalized distance is below EPS.
    assert!(
        cosmetic_frac < EPS_FRAC,
        "cosmetic edit must fall below the skip threshold: d/D = {cosmetic_frac:.4} \
         !< EPS = {EPS_FRAC}. Divergence requires the locality-preserving stalk \
         to treat the cosmetic edit as close.",
    );

    // Sanity: the cosmetic distance must sit well below the orthogonality floor,
    // else the representation is behaving like an avalanche hash and "close" is
    // vacuous.
    assert!(
        cosmetic_frac < orth_floor / 2.0,
        "cosmetic d/D ({cosmetic_frac:.4}) must be far below the orthogonality \
         floor ({orth_floor:.4}); otherwise the stalk is not locality-preserving",
    );
}

/// RUNG-2 SMOKE TEST — the red flag, asserted as the honest finding rather than
/// papered over. Surface byte-trigram distance does NOT track derived-fact
/// stability: a semantically cosmetic rename (zero fact change) moves the stalk
/// as far as, or farther than, a fact-changing edit (new callee + new CFG
/// branch). ADR-0030 predicts exactly this for rung 2; it is previewed here.
///
/// The test pins the INVERSION so a future representation change that fixed it
/// would surface (this assertion would then fail, prompting a rung-2 re-run).
#[test]
fn surface_distance_does_not_track_fact_stability_rung2_preview() {
    let before = hv_stalk(BEFORE.as_bytes());
    let rename = frac(&before, &hv_stalk(AFTER_RENAME.as_bytes())); // cosmetic
    let meaningful = frac(&before, &hv_stalk(AFTER_MEANINGFUL.as_bytes())); // fact change

    let big_before = hv_stalk(BIG_BEFORE.as_bytes());
    let big_rename = frac(&big_before, &hv_stalk(BIG_RENAME.as_bytes())); // cosmetic
    let big_meaningful = frac(&big_before, &hv_stalk(BIG_MEANINGFUL.as_bytes())); // fact change

    eprintln!("--- Rung-2 smoke test (RED): surface distance ⊥ fact stability ---");
    eprintln!(
        "small region: cosmetic rename d/D = {rename:.4}  vs  \
         fact-changing edit d/D = {meaningful:.4}",
    );
    eprintln!(
        "big region:   cosmetic rename d/D = {big_rename:.4}  vs  \
         fact-changing edit d/D = {big_meaningful:.4}",
    );
    eprintln!(
        "RED FLAG: a rename (no fact change) moves the stalk >= a fact-changing \
         edit. Surface magnitude tracks trigram churn, not derived-fact stability. \
         Byte-level stalk previews rung-2 failure; the rename-invariant \
         AST-structural stalk (needs a parser) is the rung-2 candidate.",
    );

    // The inversion, pinned. Both regions independently show the cosmetic
    // rename moving the stalk at least as far as the fact-changing edit.
    assert!(
        rename >= meaningful,
        "expected the rung-2 inversion (cosmetic rename >= fact-changing edit); \
         got rename d/D = {rename:.4}, meaningful d/D = {meaningful:.4}. If this \
         flips, byte-surface distance started tracking fact stability — re-run rung 2.",
    );
    assert!(
        big_rename > big_meaningful,
        "expected the rung-2 inversion in the big region too; got rename d/D = \
         {big_rename:.4}, meaningful d/D = {big_meaningful:.4}.",
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Measured `d/D` for a pair of independent hypervectors — the representation's
/// orthogonality floor at this D. Two unrelated regions land here.
fn independent_pair_frac() -> f64 {
    let a = bytes_to_hv(b"rung1-independent-a");
    let b = bytes_to_hv(b"rung1-independent-b");
    frac(&a, &b)
}

fn hex8(bytes: &[u8; 32]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}
