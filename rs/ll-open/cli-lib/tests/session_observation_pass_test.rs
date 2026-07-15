//! ADR-0020 §1 Gate 1 — fixture test for `SessionObservationPass`.
//!
//! Bead `ley-line-open-c7c79a` (L8). The pass is the implementation
//! that turns Gate 1 (`"One pass writes observations"`) from prose
//! into a falsifiable behavior. This integration test ingests a
//! 5-turn fixture session JSONL and asserts:
//!
//! 1. The pass creates the `observation` schema (table + 2 indices).
//! 2. Row count = 5 (one per turn).
//! 3. The `mentions` JSON arrays contain every observer-emitted
//!    token shape ADR-0020 §1 calls out: paths,
//!    `<path>:sym:<NAME>`, bare `sym:NAME`, `bead:ID`, `commit:SHA`.
//! 4. `payload_kind = "agent.session_turn"` and `source = "claude-code"`
//!    on every row.
//! 5. The watermark in `_meta.session_observation_last_ms` advances
//!    to the highest `observed_at`.
//! 6. A second `run()` over the same corpus is a no-op (watermark
//!    short-circuits all turns).

use parking_lot::Mutex;
use std::path::PathBuf;

use leyline_cli_lib::daemon::enrichment::EnrichmentPass;
use leyline_cli_lib::daemon::session_observation_pass::SessionObservationPass;
use rusqlite::Connection;
use tempfile::TempDir;

/// Path to the gate-1 fixture, resolved at compile time via
/// `CARGO_MANIFEST_DIR`. Pins fixture location to the source tree so
/// the test runs from any cwd.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("session_5turn.jsonl")
}

/// Stage the fixture in a fresh corpus directory and set the corpus
/// env var to point at it. The env var is process-global; tests that
/// touch it serialize through `corpus_env_lock()`.
fn stage_corpus(td: &TempDir) -> PathBuf {
    let corpus = td.path().join("sessions");
    std::fs::create_dir_all(&corpus).unwrap();
    let dst = corpus.join("session_5turn.jsonl");
    std::fs::copy(fixture_path(), &dst).unwrap();
    corpus
}

/// Process-global lock around `LEYLINE_SESSION_CORPUS`. Cargo runs
/// tests within a binary in parallel by default; the env var is
/// per-process so two tests racing on it would observe each other's
/// values. Hold the lock across the entire `run()` invocation.
fn corpus_env_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Pre-create `_meta` so the watermark `get_meta` / `set_meta` calls
/// have a backing table. Mirrors what the daemon does at startup.
fn meta_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    conn
}

#[test]
fn gate_1_ingests_five_turns_and_extracts_mentions() {
    let _env_guard = corpus_env_lock().lock();
    let td = TempDir::new().unwrap();
    let corpus = stage_corpus(&td);
    // SAFETY: tests serialize through corpus_env_lock; no other
    // thread reads the env var concurrently.
    unsafe {
        std::env::set_var("LEYLINE_SESSION_CORPUS", &corpus);
    }

    let conn = meta_conn();
    let pass = SessionObservationPass::new();
    let stats = pass
        .run(&conn, td.path(), None)
        .expect("SessionObservationPass::run");

    // Gate-1: row count == 5.
    assert_eq!(
        stats.items_added, 5,
        "expected 5 turns ingested, got {stats:?}",
    );
    assert_eq!(stats.files_processed, 1, "one JSONL file: {stats:?}");

    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_count, 5);

    // Every row tagged with the canonical source + payload_kind.
    let (source_count, kind_count): (i64, i64) = conn
        .query_row(
            "SELECT \
               SUM(CASE WHEN source = 'claude-code' THEN 1 ELSE 0 END), \
               SUM(CASE WHEN payload_kind = 'agent.session_turn' THEN 1 ELSE 0 END) \
             FROM observation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(source_count, 5, "all rows tagged source=claude-code");
    assert_eq!(kind_count, 5, "all rows tagged kind=agent.session_turn");

    // All payloads inline (fixture is far below INLINE_THRESHOLD).
    let inline_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observation WHERE payload_inline IS NOT NULL AND payload_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inline_count, 5, "all fixture payloads must be inline");

    // Collect every mention token across all rows.
    let mut all_mentions: Vec<String> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT mentions FROM observation ORDER BY observed_at")
        .unwrap();
    for row in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        let json: serde_json::Value = serde_json::from_str(&row.unwrap()).unwrap();
        for v in json.as_array().expect("mentions is JSON array") {
            all_mentions.push(v.as_str().unwrap().to_string());
        }
    }

    // The fixture cites these canonical tokens (one per shape ×
    // several instances). Gate 1 requires `mentions` to recover
    // them all — drift in the extractor surfaces here.
    let expected = [
        "rs/ll-open/sheaf/src/lib.rs:sym:CellComplex",
        "rs/ll-open/sheaf/src/cellcomplex.rs:sym:detect_violations",
        "rs/ll-open/cli-lib/src/daemon/enrichment.rs",
        "sym:EnrichmentPass",
        "bead:ley-line-open-8bf731",
        "bead:ley-line-open-79a37c",
        "commit:962c8e8",
    ];
    for tok in expected {
        assert!(
            all_mentions.iter().any(|m| m == tok),
            "expected mention `{tok}` missing from extracted set {all_mentions:?}",
        );
    }

    // observed_at distinct + monotonically increasing per fixture.
    let timestamps: Vec<i64> = conn
        .prepare("SELECT observed_at FROM observation ORDER BY observed_at")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        timestamps,
        vec![
            1_750_000_000_000,
            1_750_000_005_000,
            1_750_000_010_000,
            1_750_000_015_000,
            1_750_000_020_000,
        ],
        "observed_at values must round-trip",
    );

    // Watermark advanced to max observed_at.
    let watermark = leyline_ts::schema::get_meta(&conn, "session_observation_last_ms")
        .unwrap()
        .expect("watermark must be set after ingest");
    assert_eq!(watermark, "1750000020000");

    // SAFETY: same justification as the set above — env var is
    // serialized through corpus_env_lock.
    unsafe {
        std::env::remove_var("LEYLINE_SESSION_CORPUS");
    }
}

#[test]
fn gate_1_second_run_is_idempotent() {
    // Re-ingesting the same corpus must not duplicate rows — the
    // watermark short-circuits every turn at-or-below the previous
    // max observed_at. Without this, every daemon reparse would
    // double the observation row count.
    let _env_guard = corpus_env_lock().lock();
    let td = TempDir::new().unwrap();
    let corpus = stage_corpus(&td);
    // SAFETY: serialized through corpus_env_lock.
    unsafe {
        std::env::set_var("LEYLINE_SESSION_CORPUS", &corpus);
    }

    let conn = meta_conn();
    let pass = SessionObservationPass::new();

    let first = pass.run(&conn, td.path(), None).unwrap();
    assert_eq!(first.items_added, 5);

    let second = pass.run(&conn, td.path(), None).unwrap();
    assert_eq!(
        second.items_added, 0,
        "second run must short-circuit via watermark, got {second:?}",
    );

    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_count, 5, "row count must stay at 5 after re-run");

    unsafe {
        std::env::remove_var("LEYLINE_SESSION_CORPUS");
    }
}

#[test]
fn no_corpus_env_var_is_a_noop() {
    // Daemons running outside an interactive Claude Code session
    // have no corpus to ingest. The pass must register cleanly and
    // return zero work, not error. Pin so a refactor that demanded
    // the env var would break the open-edition daemon's default
    // boot path.
    let _env_guard = corpus_env_lock().lock();
    let td = TempDir::new().unwrap();
    // SAFETY: serialized through corpus_env_lock.
    unsafe {
        std::env::remove_var("LEYLINE_SESSION_CORPUS");
    }

    let conn = meta_conn();
    let pass = SessionObservationPass::new();
    let stats = pass.run(&conn, td.path(), None).unwrap();
    assert_eq!(stats.items_added, 0);
    assert_eq!(stats.files_processed, 0);
}
