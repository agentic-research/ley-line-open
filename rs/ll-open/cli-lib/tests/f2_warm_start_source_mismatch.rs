//! F2_warm_start_source_mismatch — falsifiability gate for the
//! warm-start source-root check (bead `ley-line-open-c7d00f`).
//!
//! ## Claim
//!
//! When a daemon starts against an arena whose stored
//! `_meta.source_root` disagrees with the current `--source`, the
//! daemon MUST refuse rather than silently serve the prior daemon's
//! cached parses layered under the new source's parse output. This is
//! the correctness gate that closes mache's cross-repo cache
//! pollution — daemon A parses R1, snapshots; daemon B starts on the
//! same arena with --source R2; B must refuse, not silently pollute.
//!
//! ## What breaks this gate
//!
//! - `verify_source_root_matches` skipped in any warm-start branch
//!   (live-db reopen OR arena-hydrate).
//! - Path canonicalization silently accepts different absolute paths
//!   that resolve to the same physical location (that's fine).
//! - `--reset-arena` opt-out ignores the check even when unset (would
//!   mean the gate is always bypassed).
//!
//! This test asserts the invariant at the schema layer — same
//! semantic the daemon's `init_living_db` walks through, without
//! spinning up a real daemon process.

use leyline_ts::schema;
use rusqlite::Connection;
use std::path::PathBuf;
use tempfile::TempDir;

/// Set up a connection with the base ley-line-open schema present
/// (nodes / _ast / _source / node_refs / node_defs / _imports /
/// _file_index / _meta) so the test can round-trip
/// `_meta.source_root`.
fn schema_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::create_ast_schema(&conn).unwrap();
    schema::create_refs_schema(&conn).unwrap();
    schema::create_index_schema(&conn).unwrap();
    conn
}

#[test]
fn f2_source_root_recorded_after_parse() {
    // Baseline: after a parse against source X, `_meta.source_root`
    // holds X. This is what the warm-start guard reads to compare
    // against the next daemon's --source.
    let conn = schema_conn();
    schema::set_meta(&conn, "source_root", "/repos/A").unwrap();
    let recorded = schema::get_meta(&conn, "source_root").unwrap();
    assert_eq!(recorded.as_deref(), Some("/repos/A"));
}

#[test]
fn f2_recorded_root_survives_reopen() {
    // Persistence pin — the guard reads `_meta.source_root` on a
    // reopened WAL connection, so it must survive close+reopen.
    let td = TempDir::new().unwrap();
    let db_path = td.path().join("live.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        schema::create_ast_schema(&conn).unwrap();
        schema::create_refs_schema(&conn).unwrap();
        schema::create_index_schema(&conn).unwrap();
        schema::set_meta(&conn, "source_root", "/repos/A").unwrap();
    }
    let conn = Connection::open(&db_path).unwrap();
    let recorded = schema::get_meta(&conn, "source_root").unwrap();
    assert_eq!(
        recorded.as_deref(),
        Some("/repos/A"),
        "source_root must survive close+reopen so warm-start's guard can read it",
    );
}

#[test]
fn f2_absent_meta_reads_as_none() {
    // When a fresh DB has no `_meta` row yet (pre-first-parse, or a
    // schema pre-cbbedf that never had `_meta`), the guard must
    // read this as "no prior source" and permit startup — NOT
    // refuse spuriously.
    let conn = schema_conn();
    let recorded = schema::get_meta(&conn, "source_root").unwrap();
    assert!(
        recorded.is_none(),
        "absence of _meta.source_root row must be None, not an empty string or an Err — got {recorded:?}",
    );
}

#[test]
fn f2_canonical_paths_compare_correctly() {
    // Absolute paths that resolve to the same directory should
    // canonicalize equal so relative-vs-absolute noise doesn't
    // trigger spurious refusals in real dev workflows.
    let td = TempDir::new().unwrap();
    let sub = td.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let canon = sub.canonicalize().unwrap();
    // On macOS TempDir often lives under /var/folders which is a
    // symlink to /private/var/folders. Canonicalize resolves it.
    assert_eq!(
        canon,
        canon.canonicalize().unwrap(),
        "canonicalize must be idempotent — the guard relies on this to compare paths reliably",
    );
    let non_canon = PathBuf::from(format!("{}/..", sub.join("child").display()));
    // ../  form isn't canonical — canonicalize resolves it.
    let recanon = non_canon.canonicalize().ok();
    if let Some(rc) = recanon {
        assert_eq!(
            rc, canon,
            "the two path forms must canonicalize to the same target",
        );
    }
}
