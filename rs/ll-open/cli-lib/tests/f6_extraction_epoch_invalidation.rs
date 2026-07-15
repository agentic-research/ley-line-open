//! F6_extraction_epoch_invalidation — falsifiability gate for
//! extraction-rules epoch invalidation (bead `ley-line-open-20988a`).
//!
//! ## Claim
//!
//! `node_defs` / `node_refs` / `_imports` are derived facts keyed on
//! `node_hash`, a fold over source BYTES only. When the extraction
//! rules change (a binary upgrade changing `extract_*` emission — this
//! happened at v0.7.8 with keyed_element/argument_list refs) but the
//! source bytes don't, `node_hash` is unchanged, the mtime+size skip
//! keeps every file "unchanged", and an existing arena serves silently
//! stale derived facts forever. The parse layer must record the
//! extraction epoch that produced the current facts and force full
//! re-derivation when the binary's epoch disagrees.
//!
//! ## What breaks this gate
//!
//! - Epoch not recorded in `_meta` after a parse pass.
//! - Epoch mismatch not overriding the mtime+size unchanged-skip.
//! - Pre-epoch arenas (no `_meta.extraction_epoch` row — every arena
//!   written before this gate existed) treated as current instead of
//!   stale.
//! - Epoch check breaking the same-epoch incremental fast path.
//!
//! ## Injection
//!
//! `LLO_EXTRACTION_EPOCH` overrides the compile-time epoch so a single
//! test binary can act as two releases with different extraction
//! rules. Same env-injection convention as `LLO_PERF_GATES`.

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serializes tests that mutate `LLO_EXTRACTION_EPOCH` — env vars are
/// process-global and the default test harness runs tests on parallel
/// threads. Poisoning is tolerated: a panicking test leaves no env
/// state behind (`EpochOverride` restores on drop), so later tests
/// must still run and fail on their own assertions.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores the prior `LLO_EXTRACTION_EPOCH` on drop so an assert
/// failure in one test can't leak an override into another.
struct EpochOverride {
    prev: Option<String>,
}

impl EpochOverride {
    const KEY: &'static str = "LLO_EXTRACTION_EPOCH";

    fn set(value: &str) -> Self {
        let prev = std::env::var(Self::KEY).ok();
        // SAFETY: callers hold ENV_LOCK, so no other thread in this
        // test binary touches the env concurrently.
        unsafe { std::env::set_var(Self::KEY, value) };
        Self { prev }
    }
}

impl Drop for EpochOverride {
    fn drop(&mut self) {
        // SAFETY: same ENV_LOCK scope as `set`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(Self::KEY, v),
                None => std::env::remove_var(Self::KEY),
            }
        }
    }
}

/// Small Go corpus exercising func/method/type/import extraction —
/// same shape as the F4 determinism fixture. Written once; never
/// touched again, so mtime+size stay constant across every reparse in
/// a test. That constancy is the point: only an epoch mismatch may
/// force re-derivation here.
fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "\
package main

import (
\t\"fmt\"
\t\"strings\"
)

type Greeter struct {
\tName string
}

func (g *Greeter) Hello() string {
\treturn fmt.Sprintf(\"Hello, %s!\", g.Name)
}

func normalizeName(name string) string {
\treturn strings.TrimSpace(name)
}

func main() {
\tg := &Greeter{Name: \"world\"}
\tfmt.Println(g.Hello())
\t_ = normalizeName(\"  hi  \")
}
",
    )
    .unwrap();
    fs::write(
        td.path().join("util.go"),
        "\
package main

import \"strings\"

func upper(s string) string {
\treturn strings.ToUpper(s)
}

func lower(s string) string {
\treturn strings.ToLower(s)
}
",
    )
    .unwrap();
    td
}

/// Parse `repo` into the file-backed db at `db_path` on a fresh
/// connection, mirroring the daemon's warm-start shape (reopen the
/// arena's db, run a full-tree pass). Returns the `ParseResult` so
/// tests can assert parsed/unchanged counts.
fn parse_pass(db_path: &Path, repo: &Path) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("go"), None).unwrap()
}

/// Per-(token) ref counts, sorted — order-insensitive snapshot of the
/// derived `node_refs` facts.
fn refs_snapshot(db_path: &Path) -> Vec<(String, i64)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT token, COUNT(*) FROM node_refs GROUP BY token ORDER BY token")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

fn stored_epoch(db_path: &Path) -> Option<String> {
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::get_meta(&conn, "extraction_epoch").unwrap()
}

fn execute(db_path: &Path, sql: &str) -> usize {
    let conn = Connection::open(db_path).unwrap();
    conn.execute(sql, []).unwrap()
}

#[test]
fn f6_epoch_recorded_after_parse() {
    // Baseline provenance: every full-tree parse must stamp which
    // extraction epoch produced the current derived facts. Without
    // this row a later binary has nothing to compare against.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EpochOverride::set("41");
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());
    assert_eq!(
        stored_epoch(&db_path).as_deref(),
        Some("41"),
        "_meta.extraction_epoch must record the epoch that produced the derived facts",
    );
}

#[test]
fn f6_epoch_mismatch_forces_full_rederivation() {
    // The v0.7.8 scenario made executable: binary A (epoch 1) builds
    // the arena; binary B (epoch 2, different extraction rules) adopts
    // it warm with byte-identical sources. B must re-derive every
    // file's facts — the mtime+size skip must not apply.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    {
        let _g = EpochOverride::set("1");
        parse_pass(&db_path, repo.path());
    }
    let baseline = refs_snapshot(&db_path);
    assert!(
        baseline.len() > 3,
        "fixture must produce >3 distinct ref tokens; got {baseline:?}",
    );

    // Simulate stale derived facts: an older extractor's output
    // differs from what the current rules would emit. Emptying the
    // table is the extreme form — any re-derivation restores it, and
    // no mtime+size skip can.
    execute(&db_path, "DELETE FROM node_refs");
    assert!(refs_snapshot(&db_path).is_empty());

    let result = {
        let _g = EpochOverride::set("2");
        parse_pass(&db_path, repo.path())
    };

    assert_eq!(
        result.unchanged, 0,
        "epoch mismatch must override the mtime+size unchanged-skip; \
         got {} unchanged / {} parsed",
        result.unchanged, result.parsed,
    );
    assert_eq!(
        result.parsed, 2,
        "epoch mismatch must reparse every file; got {} parsed",
        result.parsed,
    );
    assert_eq!(
        refs_snapshot(&db_path),
        baseline,
        "derived node_refs must be re-derived (restored to rule output) after epoch bump",
    );
    assert_eq!(
        stored_epoch(&db_path).as_deref(),
        Some("2"),
        "the stored epoch must advance to the binary's epoch after re-derivation",
    );
}

#[test]
fn f6_pre_epoch_arena_rederives() {
    // Every arena written before this gate has no
    // `_meta.extraction_epoch` row. Those arenas predate the epoch
    // mechanism entirely, so their derived facts are of unknown
    // provenance — treat as stale, not current.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EpochOverride::set("3");
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());
    let baseline = refs_snapshot(&db_path);
    execute(&db_path, "DELETE FROM _meta WHERE key = 'extraction_epoch'");
    execute(&db_path, "DELETE FROM node_refs");

    let result = parse_pass(&db_path, repo.path());
    assert_eq!(
        (result.parsed, result.unchanged),
        (2, 0),
        "an arena with no recorded epoch must fully re-derive; got {} parsed / {} unchanged",
        result.parsed,
        result.unchanged,
    );
    assert_eq!(
        refs_snapshot(&db_path),
        baseline,
        "pre-epoch arena's node_refs must be re-derived on adoption",
    );
    assert_eq!(stored_epoch(&db_path).as_deref(), Some("3"));
}

#[test]
fn f6_same_epoch_preserves_incremental_skip() {
    // Guard the fast path: when the stored epoch matches the binary's,
    // the mtime+size skip must keep working — the epoch check may not
    // degrade warm reparse to a cold parse on every startup.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EpochOverride::set("7");
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    let first = parse_pass(&db_path, repo.path());
    assert_eq!(first.parsed, 2, "cold pass must parse both fixture files");

    let second = parse_pass(&db_path, repo.path());
    assert_eq!(
        (second.parsed, second.unchanged),
        (0, 2),
        "same-epoch reparse of untouched sources must skip both files; got {} parsed / {} unchanged",
        second.parsed,
        second.unchanged,
    );
}
