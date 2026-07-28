//! F8_ir_schema_version_invalidation — falsifiability gate for
//! `node_hash` address-lineage invalidation (beads `ley-line-open-25811f`,
//! `ley-line-open-2037b4`; ADR-0027 §Consequences, ADR-0034 D4).
//!
//! ## Claim
//!
//! `node_hash` is a fold whose preimage carries the **canonical κ kind**, not
//! the raw tree-sitter kind — `canonical_kind(raw).unwrap_or(raw)` in
//! `cmd_parse`. So changing the κ map changes `node_hash` for every node of the
//! remapped kind, for byte-identical sources.
//!
//! That is exactly the F6 shape one level down. F6 guards derived FACTS going
//! stale when extraction rules change; this guards derived ADDRESSES going stale
//! when the κ map does. The mtime+size skip cannot see either: the bytes did not
//! move.
//!
//! ADR-0027 named `_meta.ir_schema_version` as the lineage marker for exactly
//! this, and ADR-0034 D4 states that a κ-map change is a generation-lineage
//! event rather than a silent reinterpretation.
//!
//! ## Why this gate exists at all
//!
//! It was written because the marker was **write-only**. `cmd_parse` set
//! `ir_schema_version` and nothing on earth read it — not the incremental guard,
//! not a test, not a consumer. A lineage marker nobody reads announces the break
//! to nobody, which is the same unwired-capability shape as
//! `ley-line-open-918a75` (a feature compiled out of every build) and
//! `ley-line-open-4ec276` (a generator wired into no target).
//!
//! The concrete cost: `ley-line-open-25811f` moved `function_signature_item`
//! from the raw kind to κ `function`, rewriting every trait-signature
//! `node_hash`. Without this gate, an arena built before that change would keep
//! serving pre-change addresses forever, and a consumer pinning `node_hash` had
//! no way to ask whether its cached addresses were still comparable.
//!
//! ## What breaks this gate
//!
//! - `ir_schema_version` not recorded in `_meta` after a parse pass.
//! - A recorded version that disagrees with the binary's not overriding the
//!   mtime+size unchanged-skip.
//! - Pre-marker arenas (no `_meta.ir_schema_version` row) treated as current
//!   instead of stale.
//! - The check breaking the same-version incremental fast path.

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// A prior lineage. Any value that is not the binary's current
/// `IR_SCHEMA_VERSION` exercises the mismatch path; `merkle-ast-v1` is the
/// real one this release supersedes.
const SUPERSEDED_LINEAGE: &str = "merkle-ast-v1";

fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc greet() string {\n\treturn \"hi\"\n}\n\nfunc main() {\n\tfmt.Println(greet())\n}\n",
    )
    .unwrap();
    fs::write(
        td.path().join("util.go"),
        "package main\n\nimport \"strings\"\n\nfunc upper(s string) string {\n\treturn strings.ToUpper(s)\n}\n",
    )
    .unwrap();
    td
}

fn parse_pass(db_path: &Path, repo: &Path) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("go"), None).unwrap()
}

fn stored_lineage(db_path: &Path) -> Option<String> {
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::get_meta(&conn, "ir_schema_version").unwrap()
}

fn set_lineage(db_path: &Path, value: &str) {
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::set_meta(&conn, "ir_schema_version", value).unwrap();
}

fn execute(db_path: &Path, sql: &str) -> usize {
    let conn = Connection::open(db_path).unwrap();
    conn.execute(sql, []).unwrap()
}

fn defs_count(db_path: &Path) -> i64 {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn f8_ir_schema_version_recorded_after_parse() {
    // Baseline provenance: a parse must stamp WHICH address lineage produced
    // the current node_hash values. Without this row a later binary has nothing
    // to compare against, and the marker cannot invalidate anything.
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());

    let recorded = stored_lineage(&db_path);
    assert!(
        recorded.is_some(),
        "every parse must record _meta.ir_schema_version; got None",
    );
    assert_ne!(
        recorded.as_deref(),
        Some(SUPERSEDED_LINEAGE),
        "the binary must record its CURRENT lineage, not the superseded one \
         this release replaced",
    );
}

#[test]
fn f8_lineage_mismatch_forces_full_rederivation() {
    // The ley-line-open-25811f scenario made executable: an arena built under
    // the old κ map holds node_hash values that the current map would not
    // produce. Sources are byte-identical, so mtime+size says "unchanged" —
    // the lineage marker is the only thing that can force re-derivation.
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());
    let baseline = defs_count(&db_path);
    assert!(
        baseline > 0,
        "fixture must produce node_defs rows; got {baseline}",
    );

    // Simulate an arena written under the superseded lineage. Emptying the
    // derived table is the extreme form of "these addresses are wrong": any
    // re-derivation restores it, and no mtime+size skip can.
    set_lineage(&db_path, SUPERSEDED_LINEAGE);
    execute(&db_path, "DELETE FROM node_defs");
    assert_eq!(defs_count(&db_path), 0);

    let result = parse_pass(&db_path, repo.path());

    assert_eq!(
        result.unchanged, 0,
        "lineage mismatch must override the mtime+size unchanged-skip; \
         got {} unchanged / {} parsed",
        result.unchanged, result.parsed,
    );
    assert_eq!(
        defs_count(&db_path),
        baseline,
        "derived addresses must be re-derived after a lineage change",
    );
    assert_ne!(
        stored_lineage(&db_path).as_deref(),
        Some(SUPERSEDED_LINEAGE),
        "the stored lineage must advance to the binary's after re-derivation",
    );
}

#[test]
fn f8_pre_marker_arena_is_treated_as_stale() {
    // Every arena written before the marker existed has NO ir_schema_version
    // row. A missing row must read as "unknown lineage", i.e. stale — not as
    // "current". Treating absence as agreement is how a silent-staleness bug
    // survives a migration.
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());
    let baseline = defs_count(&db_path);

    execute(
        &db_path,
        "DELETE FROM _meta WHERE key = 'ir_schema_version'",
    );
    execute(&db_path, "DELETE FROM node_defs");
    assert_eq!(stored_lineage(&db_path), None);

    let result = parse_pass(&db_path, repo.path());

    assert_eq!(
        result.unchanged, 0,
        "a missing lineage row must be treated as stale, not current; \
         got {} unchanged",
        result.unchanged,
    );
    assert_eq!(defs_count(&db_path), baseline);
}

#[test]
fn f8_same_lineage_preserves_incremental_skip() {
    // The guard must not defeat the fast path it guards. Two consecutive
    // parses at the same lineage, with untouched sources, must still skip.
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());
    let second = parse_pass(&db_path, repo.path());

    assert_eq!(
        second.parsed, 0,
        "same lineage + untouched sources must keep the unchanged-skip; \
         got {} parsed",
        second.parsed,
    );
    assert!(
        second.unchanged > 0,
        "expected files to be skipped as unchanged; got {}",
        second.unchanged,
    );
}
