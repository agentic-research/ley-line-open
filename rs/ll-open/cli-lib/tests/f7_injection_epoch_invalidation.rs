//! F7_injection_epoch_invalidation — falsifiability gate for the
//! COMPOSITE injection epoch (bead `ley-line-open-c822a6`, extending
//! the f6 extraction-epoch gate from bead `ley-line-open-20988a`).
//!
//! ## Claim
//!
//! Injected facts depend on inputs the scalar `EXTRACTION_EPOCH` does
//! not see: the host language's `injections.scm`, the injected
//! language's `tags.scm`, and both grammars. The parse layer stores a
//! composite `_meta.injection_epoch` — a σ hash over
//! {extraction epoch, injections.scm bytes, injected tags.scm bytes,
//! grammar fingerprints} — and ANDs it with `extraction_epoch` in the
//! unchanged-skip gate. A change to any composite input with
//! byte-identical sources must force full fact re-derivation.
//!
//! ## What breaks this gate
//!
//! - `injection_epoch` not recorded in `_meta` after a full-tree pass.
//! - An `injections.scm` content change not overriding the mtime+size
//!   unchanged-skip.
//! - Pre-injection arenas (no `_meta.injection_epoch` row) treated as
//!   current instead of stale.
//! - The composite check degrading the same-epoch incremental fast
//!   path.
//!
//! ## Injection
//!
//! `LLO_INJECTIONS_SCM` substitutes the `injections.scm` bytes in the
//! composite's preimage so one test binary can act as two releases
//! shipping different injection queries. Same env-injection convention
//! as `LLO_EXTRACTION_EPOCH` (f6): the override changes the epoch
//! INPUT, not the emission — the gate under test is invalidation, not
//! extraction.

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores the prior `LLO_INJECTIONS_SCM` on drop.
struct ScmOverride {
    prev: Option<String>,
}

impl ScmOverride {
    const KEY: &'static str = "LLO_INJECTIONS_SCM";

    fn set(value: &str) -> Self {
        let prev = std::env::var(Self::KEY).ok();
        // SAFETY: callers hold ENV_LOCK, so no other thread in this
        // test binary touches the env concurrently.
        unsafe { std::env::set_var(Self::KEY, value) };
        Self { prev }
    }
}

impl Drop for ScmOverride {
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

/// Go fixture with one SQL injection site, so re-derivation assertions
/// cover INJECTED facts too, not just host-native ones. Written once;
/// mtime+size stay constant across every reparse in a test.
fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "\
package main

import \"database/sql\"

func setup(db *sql.DB) {
\tdb.Exec(`CREATE TABLE users (id INTEGER, name TEXT)`)
}

func helper() string {
\treturn \"plain\"
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
",
    )
    .unwrap();
    td
}

fn parse_pass(db_path: &Path, repo: &Path) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("go"), None).unwrap()
}

/// Order-insensitive snapshot of the derived def facts, injected rows
/// included.
fn defs_snapshot(db_path: &Path) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT token, node_id FROM node_defs ORDER BY token, node_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn stored_injection_epoch(db_path: &Path) -> Option<String> {
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::get_meta(&conn, "injection_epoch").unwrap()
}

fn execute(db_path: &Path, sql: &str) -> usize {
    let conn = Connection::open(db_path).unwrap();
    conn.execute(sql, []).unwrap()
}

#[test]
fn f7_injection_epoch_recorded_after_parse() {
    // Baseline provenance: every full-tree parse must stamp the
    // composite injection epoch that produced the current facts.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());
    let stored = stored_injection_epoch(&db_path);
    assert!(
        stored.as_deref().is_some_and(|s| !s.is_empty()),
        "_meta.injection_epoch must record the composite that produced the derived facts; \
         got {stored:?}",
    );
}

#[test]
fn f7_injections_scm_change_forces_full_rederivation() {
    // Release A ships injections.scm v1; release B ships v2. Sources
    // are byte-identical, mtime+size unchanged — only the composite
    // disagrees. B must re-derive every file's facts, injected facts
    // included.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    let epoch_v1 = {
        let _g = ScmOverride::set("; injections v1");
        parse_pass(&db_path, repo.path());
        stored_injection_epoch(&db_path).expect("v1 pass must stamp injection_epoch")
    };
    let baseline = defs_snapshot(&db_path);
    assert!(
        baseline.iter().any(|(t, _)| t == "users"),
        "fixture must produce the injected `users` def; got {baseline:?}",
    );

    // Simulate stale derived facts: only a re-derivation pass can
    // restore them — no mtime+size skip can.
    execute(&db_path, "DELETE FROM node_defs");
    assert!(defs_snapshot(&db_path).is_empty());

    let result = {
        let _g = ScmOverride::set("; injections v2");
        parse_pass(&db_path, repo.path())
    };

    assert_eq!(
        result.unchanged, 0,
        "injections.scm change must override the mtime+size unchanged-skip; \
         got {} unchanged / {} parsed",
        result.unchanged, result.parsed,
    );
    assert_eq!(
        result.parsed, 2,
        "injections.scm change must reparse every file; got {} parsed",
        result.parsed,
    );
    assert_eq!(
        defs_snapshot(&db_path),
        baseline,
        "derived node_defs (injected rows included) must be re-derived after the composite change",
    );
    let epoch_v2 = stored_injection_epoch(&db_path).expect("v2 pass must stamp injection_epoch");
    assert_ne!(
        epoch_v1, epoch_v2,
        "different injections.scm bytes must produce different composites",
    );
}

#[test]
fn f7_pre_injection_arena_rederives() {
    // Every arena written before this gate has no
    // `_meta.injection_epoch` row — its facts predate injections
    // entirely (no injected rows exist). Treat as stale: the one
    // forced re-derive on upgrade is what delivers the injected facts,
    // which is why no EXTRACTION_EPOCH bump accompanies this feature.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    parse_pass(&db_path, repo.path());
    let baseline = defs_snapshot(&db_path);
    execute(&db_path, "DELETE FROM _meta WHERE key = 'injection_epoch'");
    execute(&db_path, "DELETE FROM node_defs");

    let result = parse_pass(&db_path, repo.path());
    assert_eq!(
        (result.parsed, result.unchanged),
        (2, 0),
        "an arena with no recorded injection_epoch must fully re-derive; \
         got {} parsed / {} unchanged",
        result.parsed,
        result.unchanged,
    );
    assert_eq!(
        defs_snapshot(&db_path),
        baseline,
        "pre-injection arena's node_defs must be re-derived on adoption",
    );
    assert!(stored_injection_epoch(&db_path).is_some());
}

#[test]
fn f7_same_injection_epoch_preserves_incremental_skip() {
    // Guard the fast path: with an unchanged composite, the mtime+size
    // skip must keep working — the injection gate may not degrade warm
    // reparse to a cold parse on every startup.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    let first = parse_pass(&db_path, repo.path());
    assert_eq!(first.parsed, 2, "cold pass must parse both fixture files");

    let second = parse_pass(&db_path, repo.path());
    assert_eq!(
        (second.parsed, second.unchanged),
        (0, 2),
        "same-composite reparse of untouched sources must skip both files; \
         got {} parsed / {} unchanged",
        second.parsed,
        second.unchanged,
    );
}
