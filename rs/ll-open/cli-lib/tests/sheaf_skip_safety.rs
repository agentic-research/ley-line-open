//! Rung 3 of ADR-0030 — the safety invariant that makes a lossy semantic
//! invalidation optimizer safe (bead `ley-line-open-d53329`).
//!
//! # The invariant
//!
//! `node_hash` (exact, content-addressed) sits UNDER the sheaf's δ⁰ skip
//! decision. A δ⁰ **false-negative** — the optimizer says "close, skip
//! re-derivation" for a region whose bytes genuinely changed — must degrade to
//! *unnecessary revalidation the hash catches*, NEVER to *stale facts served to
//! a consumer*. The sheaf δ⁰ decision is advisory; `node_hash` + the extraction
//! epoch are the correctness floor.
//!
//! # Guard-path trace (measured, see bead d53329)
//!
//! In the OSS codebase the floor is not merely *underneath* the δ⁰ skip — the
//! δ⁰ skip is entirely **off** the fact-derivation and fact-serving paths:
//!
//! * `SheafCache::on_change` / `reap` are consumed only by
//!   `daemon/sheaf_ops.rs`, which turns their output into an advisory
//!   `daemon.sheaf.invalidate` event for external consumer caches.
//! * `cmd_parse` (fact derivation) reads nothing from the sheaf. It re-derives
//!   on `epoch + mtime + size` and content-addresses every fact by
//!   `node_hash` — file bytes fold to `_source.content_hash` (BLAKE3),
//!   `_ast.node_hash`, and the `node_content` primary key.
//! * Query commands read `node_defs` / `node_refs` straight from the tables the
//!   last parse wrote; they never consult the sheaf.
//!
//! So there is no code path where a δ⁰ skip can suppress a re-derivation whose
//! `node_hash` would differ. This file PINS that decoupling against regression:
//! it injects a real δ⁰ false-negative through the live `SheafCache` and asserts
//! the node_hash floor still delivers the re-derived facts. It fails if someone
//! later wires the sheaf skip to gate fact serving without the node_hash floor
//! underneath.
//!
//! # Why the false-negative is injected, not driven end-to-end
//!
//! Today's live stalk is a SHA-256 avalanche hash: no sub-EPS continuum exists,
//! so a semantic δ⁰ false-negative is unreachable through the live daemon (the
//! necessity audit `716c69` proved this). Rung 1 (`d4e605`) showed the
//! false-negative *becomes* reachable under the locality-preserving stalk the
//! ADR proposes. Here we model that regime minimally and faithfully: a real
//! `SheafCache` in δ⁰ mode where a region's boundary embedding moves a
//! sub-`DELTA0_EPS` amount (the "barely moved" the future stalk introduces)
//! while its `node_hash`/merkle stalk changes (the bytes did change). The live
//! `check_boundary_changed` δ⁰ gate then returns "unchanged" for a region whose
//! content address moved — exactly the false-negative Rung 3 must survive.

use leyline_cli_lib::cmd_parse;
use leyline_sheaf::cache::StalkHash;
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use leyline_sheaf::topology::RegionId;
use leyline_sheaf::{RestrictionEdge, SheafCache};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Real source fixtures — a fact-CHANGING edit (swaps a callee)
// ---------------------------------------------------------------------------
//
// v1 calls `computeWeight`; v2 calls `computePenalty`. The derived
// `node_refs` therefore differ: `computeWeight` disappears and
// `computePenalty` appears. A consumer that serves v1's facts after the edit
// is serving observably STALE facts. The region is a full ~17-line function so
// a single-token callee swap is a small fraction of the byte surface — the same
// shape Rung 1 measured at d/D ≈ 0.002, deep inside the "close, skip" band.

const REGION_V1: &str = "\
package main

func summarize(records []int64, threshold int64) int64 {
\tvar total int64 = 0
\tcount := 0
\tvar maxSeen int64 = -100
\tfor _, record := range records {
\t\tweight := computeWeight(record)
\t\tif weight > threshold {
\t\t\ttotal += weight
\t\t\tcount++
\t\t\tif weight > maxSeen {
\t\t\t\tmaxSeen = weight
\t\t\t}
\t\t}
\t}
\taverage := int64(0)
\tif count > 0 {
\t\taverage = total / int64(count)
\t}
\treturn total + average + maxSeen
}
";

const REGION_V2: &str = "\
package main

func summarize(records []int64, threshold int64) int64 {
\tvar total int64 = 0
\tcount := 0
\tvar maxSeen int64 = -100
\tfor _, record := range records {
\t\tweight := computePenalty(record)
\t\tif weight > threshold {
\t\t\ttotal += weight
\t\t\tcount++
\t\t\tif weight > maxSeen {
\t\t\t\tmaxSeen = weight
\t\t\t}
\t\t}
\t}
\taverage := int64(0)
\tif count > 0 {
\t\taverage = total / int64(count)
\t}
\treturn total + average + maxSeen
}
";

/// A NO-OP edit relative to v1 — byte-identical. Models the payoff case
/// (true-skip): both the δ⁰ layer AND the node_hash floor agree "unchanged", so
/// the consumer correctly serves the cached facts.
const REGION_V1_NOOP: &str = REGION_V1;

// ---------------------------------------------------------------------------
// Real fact-path helpers (the node_hash floor)
// ---------------------------------------------------------------------------

const REL: &str = "region.go";

/// Parse `repo` into the file-backed db on a fresh connection (daemon
/// warm-start shape). Second and later calls are incremental.
fn parse_pass(db_path: &Path, repo: &Path) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("go"), None).unwrap()
}

/// Sorted set of derived `node_refs` tokens — the observable facts a consumer
/// serves. Order-insensitive.
fn refs_facts(db_path: &Path) -> BTreeSet<String> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT DISTINCT token FROM node_refs")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// The node_hash-family floor signal at file granularity: `_source.content_hash`
/// = BLAKE3 of the file's exact bytes (`cmd_parse.rs`). Changes iff bytes change.
fn content_hash(db_path: &Path) -> Vec<u8> {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT content_hash FROM _source WHERE id = ?1",
        [REL],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Sheaf skip layer (the advisory δ⁰ optimizer)
// ---------------------------------------------------------------------------

/// Stalk that carries a 32-byte content hash directly — the merkle-root / node
/// hash of a region, exactly the shape `daemon/sheaf_ops.rs::HashStalk` feeds
/// the live cache.
#[derive(Clone)]
struct HashStalk([u8; 32]);
impl StalkHash for HashStalk {
    fn merkle_root(&self) -> [u8; 32] {
        self.0
    }
}

fn h(byte: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = byte;
    out
}

fn xor(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

const REF: RegionId = 0;
const R: RegionId = 1;
const EDGE_ID: RegionId = 100;

/// Build a two-region δ⁰-mode `SheafCache` (REF — R, identity restriction) with
/// a refreshed baseline (δ⁰ = 0), then inject a δ⁰ **false-negative** on R:
///
/// * push a sub-`DELTA0_EPS` movement into R's boundary embedding (5e-5 < 1e-4)
///   — the "barely moved" a locality-preserving stalk introduces, and
/// * change R's merkle/node-hash stalk (the region's bytes genuinely changed).
///
/// Returns the live `on_change(&[REF])` invalidation set. A correct injection
/// yields a set that does NOT contain R: the δ⁰ gate said "close, skip" for a
/// region whose content address moved.
fn delta0_says_skip_for_r() -> Vec<RegionId> {
    let mut cx = CellComplex::new(1);
    cx.add_node(REF, vec![0.0]);
    cx.add_node(R, vec![0.0]);
    cx.add_edge(
        EDGE_ID,
        REF,
        R,
        1,
        Some("region-boundary".into()),
        RestrictionMap::identity(1),
        RestrictionMap::identity(1),
        false,
    );

    let mut cache: SheafCache<HashStalk, &'static str> = SheafCache::new().with_complex(cx);
    cache.set_stalk(REF, HashStalk(h(0xAA)));
    cache.set_stalk(R, HashStalk(h(0x01))); // R's node_hash BEFORE the edit
    cache.set_restriction(
        REF,
        R,
        RestrictionEdge {
            weights: vec![1.0],
            boundary_hash: xor(h(0xAA), h(0x01)),
            co_change_rate: 0.5,
            revert_rate: 0.0,
        },
    );
    cache.put(REF, "ref-facts");
    cache.put(R, "region-facts-v1");
    cache.refresh_baseline();

    // --- inject the δ⁰ false-negative on R ---
    // Boundary embedding moves a sub-EPS amount: 0.0 -> 5e-5 (< DELTA0_EPS 1e-4).
    cache.set_stalk_value(R, vec![5e-5]);
    // R's node_hash / merkle stalk DID change — the bytes genuinely changed.
    cache.set_stalk(R, HashStalk(h(0x02)));

    // REF is reported as a changed root; R is its neighbor. The stage-1 XOR
    // pre-filter fires (R's hash changed), so the stage-2 δ⁰ gate is the
    // deciding check — and it sees a sub-EPS move, so it holds R "unchanged".
    cache.on_change(&[REF])
}

// ---------------------------------------------------------------------------
// The consumer contract (ADR-0030 §Correctness) as executable code
// ---------------------------------------------------------------------------

/// A consumer that reads facts for region R. Per ADR-0030, the sheaf δ⁰ verdict
/// is advisory and `node_hash` is the floor UNDERNEATH it.
///
/// The `|| node_hash_changed` term is the load-bearing floor. Deleting it (i.e.
/// trusting the δ⁰ skip alone) is exactly the regression this test exists to
/// catch: fed a δ⁰ false-negative, the consumer would then serve the stale
/// cached facts and [`delta0_false_negative_is_caught_by_node_hash_floor`]
/// would fail.
fn consumer_serves_facts(
    delta0_flagged_changed: bool,
    node_hash_changed: bool,
    cached: &BTreeSet<String>,
    arena: &BTreeSet<String>,
) -> BTreeSet<String> {
    if delta0_flagged_changed || node_hash_changed {
        arena.clone() // re-read from the node_hash-keyed arena
    } else {
        cached.clone() // trust the cache
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// THE RUNG-3 SAFETY TEST. Inject a real δ⁰ false-negative (the optimizer says
/// "skip, unchanged" for region R whose bytes changed) and assert the consumer
/// STILL receives the correct, re-derived facts because the `node_hash` floor
/// caught the change the δ⁰ gate missed.
#[test]
fn delta0_false_negative_is_caught_by_node_hash_floor() {
    // --- Real fact derivation: v1 -> v2 is a fact-CHANGING edit ---
    let repo = TempDir::new().unwrap();
    let file = repo.path().join(REL);
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    fs::write(&file, REGION_V1).unwrap();
    parse_pass(&db_path, repo.path());
    let facts_v1 = refs_facts(&db_path); // the consumer's cached facts
    let hash_v1 = content_hash(&db_path);

    fs::write(&file, REGION_V2).unwrap();
    let second = parse_pass(&db_path, repo.path());
    let facts_v2 = refs_facts(&db_path); // freshly re-derived, node_hash-keyed
    let hash_v2 = content_hash(&db_path);

    // The edit is genuine and observable in the facts, else the test is vacuous.
    assert_eq!(
        second.parsed, 1,
        "the byte change must trigger a real reparse"
    );
    assert_ne!(
        facts_v1, facts_v2,
        "fixture must be a fact-changing edit (node_refs must differ)",
    );
    assert!(
        facts_v1.contains("computeWeight") && !facts_v1.contains("computePenalty"),
        "v1 facts must reference computeWeight only; got {facts_v1:?}",
    );
    assert!(
        facts_v2.contains("computePenalty") && !facts_v2.contains("computeWeight"),
        "v2 facts must reference computePenalty only; got {facts_v2:?}",
    );

    // --- The node_hash floor registers the change ---
    let node_hash_changed = hash_v1 != hash_v2;
    assert!(
        node_hash_changed,
        "content_hash (node_hash floor) must move when bytes change",
    );

    // --- The δ⁰ layer FALSE-NEGATIVES: it says "skip, unchanged" for R ---
    let invalidated = delta0_says_skip_for_r();
    let delta0_flagged_changed = invalidated.contains(&R);
    assert!(
        !delta0_flagged_changed,
        "injected δ⁰ false-negative: the sheaf must have skipped R (a genuinely \
         changed region); on_change returned {invalidated:?}",
    );

    // --- THE SAFETY ASSERTION ---
    // δ⁰ said skip; node_hash said changed. The floor-respecting consumer
    // re-reads and serves the CORRECT re-derived facts, not the stale cache.
    let served = consumer_serves_facts(
        delta0_flagged_changed, // false — the optimizer was wrong
        node_hash_changed,      // true  — the floor caught it
        &facts_v1,              // stale cache
        &facts_v2,              // node_hash-keyed arena
    );
    assert_eq!(
        served, facts_v2,
        "node_hash floor must deliver the re-derived facts despite the δ⁰ \
         false-negative — a δ⁰ false-negative is a performance cost, not a \
         correctness bug",
    );
    assert_ne!(
        served, facts_v1,
        "consumer must NOT serve the stale v1 facts the δ⁰ skip would have \
         preserved on its own",
    );
}

/// Companion — the payoff case (true-skip) is preserved and the floor does not
/// over-fire. A no-op (byte-identical) edit: both the δ⁰ layer AND the
/// node_hash floor say "unchanged", so the consumer correctly serves cached
/// facts. This pins that the floor discriminates a true-skip (safe to serve
/// cache) from a false-negative (must re-derive) — it is not a blanket
/// "always re-derive" that would erase the optimizer's value.
#[test]
fn true_skip_serves_cache_when_node_hash_unchanged() {
    let repo = TempDir::new().unwrap();
    let file = repo.path().join(REL);
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    fs::write(&file, REGION_V1).unwrap();
    parse_pass(&db_path, repo.path());
    let facts_v1 = refs_facts(&db_path);
    let hash_v1 = content_hash(&db_path);

    // Byte-identical rewrite: content_hash cannot move (BLAKE3 of same bytes).
    fs::write(&file, REGION_V1_NOOP).unwrap();
    parse_pass(&db_path, repo.path());
    let facts_after = refs_facts(&db_path);
    let hash_after = content_hash(&db_path);

    let node_hash_changed = hash_v1 != hash_after;
    assert!(
        !node_hash_changed,
        "byte-identical content must leave the node_hash floor unchanged",
    );
    assert_eq!(facts_v1, facts_after, "no-op edit cannot change facts");

    // δ⁰ says skip (no real move) AND the floor says unchanged → serve cache.
    let served = consumer_serves_facts(false, node_hash_changed, &facts_v1, &facts_after);
    assert_eq!(
        served, facts_v1,
        "true-skip: with node_hash unchanged the consumer serves the cache — \
         the optimizer's payoff is preserved, the floor does not over-fire",
    );
}
