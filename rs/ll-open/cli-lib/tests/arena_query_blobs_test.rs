//! arena_query_blobs — falsifiability gates for content-addressed
//! arena-resident query blobs (bead `ley-line-open-e72629`).
//!
//! ## Claim
//!
//! An arena may carry OVERRIDE tree-sitter `.scm` blobs that replace the
//! compiled-in `queries/<lang>/tags.scm` defaults for a language, behind
//! a BLAKE3-hash allowlist (operator-controlled env
//! `LLO_TRUSTED_QUERY_HASHES`). The effective query for a language is the
//! arena blob when present AND trusted, else the compiled default. Skew
//! surfaces through `_meta.query_set_epoch` (extended to cover the active
//! blob bytes). Trust, ABI, and resource bounds each fail closed:
//!
//! - untrusted/unknown-hash blob → IGNORED, one stderr line, compiled
//!   fallback (never silently to no extraction);
//! - unknown capture name in a TRUSTED blob → hard load error;
//! - pathological pattern → that file skipped, one stderr line, parse
//!   completes (never a hung parse).
//!
//! ## What breaks these gates
//!
//! - Override ignored when it should apply (trusted + allowlisted).
//! - Untrusted blob silently applied, or silently dropping to no facts.
//! - ABI-unknown capture silently no-op instead of hard error.
//! - Pathological blob hanging or corrupting other files.
//! - Swapping the active blob NOT forcing re-derivation.

use leyline_cli_lib::cmd_parse;
use leyline_ts::languages::TsLanguage;
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serializes tests that mutate the process-global env knobs
/// (`LLO_TRUSTED_QUERY_HASHES`, `LLO_QUERY_MATCH_LIMIT`). Poisoning is
/// tolerated — the guards restore on drop.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores a prior env value on drop so an assert failure can't leak an
/// override into another test.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: callers hold ENV_LOCK, so no other thread in this test
        // binary touches the env concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same ENV_LOCK scope as `set`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// One Go file: `package widget`, a type `Gadget`, two funcs. Written
/// once; mtime+size stay constant across reparses so only an epoch
/// mismatch may force re-derivation.
fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("lib.go"),
        "\
package widget

type Gadget struct {
\tName string
}

func Alpha() {}

func Beta() {}
",
    )
    .unwrap();
    td
}

/// Override .scm that emits the PACKAGE NAME as a def — a fact the
/// compiled Go default never produces (distinguishable from the default).
const OVERRIDE_PKG: &[u8] =
    b"; emission-abi-version: 1\n(package_clause (package_identifier) @name) @def\n";

/// A different valid override: emits only TYPE defs. Distinguishable from
/// both the compiled default and OVERRIDE_PKG — used for the swap gate.
const OVERRIDE_TYPE: &[u8] =
    b"; emission-abi-version: 1\n(type_spec name: (type_identifier) @name) @def\n";

/// Trusted blob using a capture name outside the emission vocabulary —
/// a hard load error.
const OVERRIDE_BAD_CAP: &[u8] =
    b"; emission-abi-version: 1\n(package_clause (package_identifier) @bogus) @def\n";

/// Trusted blob with a state-explosion pattern: two unbounded wildcard
/// runs under the file root make the split point ambiguous, so the
/// matcher accumulates in-progress states proportional to the top-level
/// child count — well past a tight match limit.
const OVERRIDE_PATHOLOGICAL: &[u8] =
    b"; emission-abi-version: 1\n(source_file (_)* @ref (_)* @def)\n";

fn blake3_hex(bytes: &[u8]) -> String {
    use leyline_core::ContentAddressed;
    bytes.hash().to_string()
}

/// Insert an override blob + `_queries` pointer into the arena for
/// `lang`, returning the blob's BLAKE3 hex (an allowlist entry).
fn install_override(db_path: &Path, lang: &str, scm: &[u8]) -> String {
    use leyline_core::ContentAddressed;
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::create_query_blob_tables(&conn).unwrap();
    let hash_bytes = scm.hash().as_bytes().to_vec();
    conn.execute(
        "INSERT OR REPLACE INTO query_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
        params![hash_bytes, scm],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO _queries (lang, kind, blob_hash) VALUES (?1, 'tags', ?2)",
        params![lang, hash_bytes],
    )
    .unwrap();
    blake3_hex(scm)
}

fn parse_pass(db_path: &Path, repo: &Path) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("go"), None).unwrap()
}

fn defs(db_path: &Path) -> Vec<String> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT token FROM node_defs ORDER BY token")
        .unwrap();
    let v = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    v
}

fn meta(db_path: &Path, key: &str) -> Option<String> {
    let conn = Connection::open(db_path).unwrap();
    leyline_ts::schema::get_meta(&conn, key).unwrap()
}

// ── (a) allowlisted arena blob overrides the compiled default ──────────

#[test]
fn a_allowlisted_override_replaces_compiled_default() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");

    // Baseline: compiled default emits func/type defs, NEVER the package
    // name.
    {
        let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", "");
        parse_pass(&db_path, repo.path());
    }
    let base = defs(&db_path);
    assert!(
        base.contains(&"Alpha".to_string()) && base.contains(&"Gadget".to_string()),
        "compiled default must emit func/type defs; got {base:?}",
    );
    assert!(
        !base.contains(&"widget".to_string()),
        "compiled default must NOT emit the package name; got {base:?}",
    );

    // Install the override, allowlist its hash, reparse.
    let hex = install_override(&db_path, "go", OVERRIDE_PKG);
    let with = {
        let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", &hex);
        parse_pass(&db_path, repo.path());
        defs(&db_path)
    };
    assert!(
        with.contains(&"widget".to_string()),
        "allowlisted override must emit the distinguishable package-name def; got {with:?}",
    );
    assert!(
        !with.contains(&"Alpha".to_string()),
        "override REPLACES the compiled default — func defs must be gone; got {with:?}",
    );
    assert_eq!(
        meta(&db_path, "query_source:go").as_deref(),
        Some(format!("arena:{hex}").as_str()),
        "active override source must be observable via _meta.query_source:<lang>",
    );
}

// ── (b) non-allowlisted blob ignored + compiled default + stderr warning

#[test]
fn b_untrusted_override_ignored_with_warning_and_compiled_fallback() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());
    install_override(&db_path, "go", OVERRIDE_PKG);

    // Resolve directly with an EMPTY allowlist — the resolver returns
    // exactly one warning and no override (compiled fallback).
    let conn = Connection::open(&db_path).unwrap();
    let empty: HashSet<String> = HashSet::new();
    let resolution = leyline_ts::query_engine::resolve_query_set(&conn, &empty).unwrap();
    assert_eq!(
        resolution.warnings.len(),
        1,
        "an untrusted override must produce exactly one warning line; got {:?}",
        resolution.warnings,
    );
    assert!(
        resolution
            .query_set
            .override_engine(TsLanguage::Go)
            .is_none(),
        "untrusted override must NOT install an engine — compiled fallback",
    );

    // End-to-end with the untrusted override present: the compiled
    // default is used, and provenance shows no arena source.
    {
        let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", "");
        parse_pass(&db_path, repo.path());
    }
    let d = defs(&db_path);
    assert!(
        d.contains(&"Alpha".to_string()) && !d.contains(&"widget".to_string()),
        "untrusted override ignored → compiled facts, never override facts; got {d:?}",
    );
    assert_eq!(
        meta(&db_path, "query_source:go"),
        None,
        "no active override → no query_source row (absence = compiled)",
    );
}

// ── (c) unknown capture name in a trusted blob = hard load error ───────

#[test]
fn c_unknown_capture_in_trusted_blob_is_hard_error() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());
    let hex = install_override(&db_path, "go", OVERRIDE_BAD_CAP);

    // Trusted (allowlisted) but ABI-invalid → resolve fails loud.
    let conn = Connection::open(&db_path).unwrap();
    let allow: HashSet<String> = [hex.clone()].into_iter().collect();
    let err = match leyline_ts::query_engine::resolve_query_set(&conn, &allow) {
        Ok(_) => panic!("trusted blob with an unknown capture must be a hard load error"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bogus") || msg.contains("emission"),
        "error must name the offending capture / ABI; got {msg}",
    );

    // The same failure propagates out of the whole parse pass.
    let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", &hex);
    let conn2 = Connection::open(&db_path).unwrap();
    assert!(
        cmd_parse::parse_into_conn(&conn2, repo.path(), Some("go"), None).is_err(),
        "a trusted ABI-invalid override must fail the parse pass, not silently no-op",
    );
}

// ── (d) pathological pattern bounded → file skipped, parse completes ───

#[test]
fn d_pathological_override_is_bounded_and_file_skipped() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());
    let hex = install_override(&db_path, "go", OVERRIDE_PATHOLOGICAL);

    // Tight match limit so the state-explosion pattern trips deterministically.
    let _lim = EnvGuard::set("LLO_QUERY_MATCH_LIMIT", "8");
    let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", &hex);

    // Parse COMPLETES (returns Ok) — never hangs.
    let conn = Connection::open(&db_path).unwrap();
    let result = cmd_parse::parse_into_conn(&conn, repo.path(), Some("go"), None)
        .expect("a pathological override must not fail the parse — it degrades to no facts");
    assert_eq!(
        result.errors, 0,
        "the file parses structurally; only facts drop"
    );

    // No facts for the bounded file.
    assert!(
        defs(&db_path).is_empty(),
        "a bounded override run must drop this file's extracted facts; got {:?}",
        defs(&db_path),
    );
}

// ── (e) swapping the active blob bumps the epoch + forces re-derivation ─

#[test]
fn e_swapping_active_blob_bumps_epoch_and_rederives() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = fixture_repo();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());

    let hex_pkg = install_override(&db_path, "go", OVERRIDE_PKG);
    let hex_type = blake3_hex(OVERRIDE_TYPE);
    // Both blobs allowlisted so the swap target is trusted too.
    let allow = format!("{hex_pkg},{hex_type}");
    let _g = EnvGuard::set("LLO_TRUSTED_QUERY_HASHES", &allow);

    // Parse under OVERRIDE_PKG.
    parse_pass(&db_path, repo.path());
    assert_eq!(
        defs(&db_path),
        vec!["widget".to_string()],
        "OVERRIDE_PKG must emit only the package-name def",
    );
    let epoch_pkg = meta(&db_path, "query_set_epoch").expect("query_set_epoch must be stamped");

    // Swap the active blob to OVERRIDE_TYPE (add the blob, repoint _queries).
    {
        let conn = Connection::open(&db_path).unwrap();
        let type_hash = {
            use leyline_core::ContentAddressed;
            OVERRIDE_TYPE.hash().as_bytes().to_vec()
        };
        conn.execute(
            "INSERT OR REPLACE INTO query_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            params![type_hash, OVERRIDE_TYPE],
        )
        .unwrap();
        conn.execute(
            "UPDATE _queries SET blob_hash = ?1 WHERE lang = 'go' AND kind = 'tags'",
            params![type_hash],
        )
        .unwrap();
    }

    // Reparse: the epoch must disagree (byte-identical sources), forcing
    // full re-derivation of every file.
    let result = parse_pass(&db_path, repo.path());
    assert_eq!(
        (result.parsed, result.unchanged),
        (1, 0),
        "swapping the active blob must override the mtime+size skip; got {} parsed / {} unchanged",
        result.parsed,
        result.unchanged,
    );
    assert_eq!(
        defs(&db_path),
        vec!["Gadget".to_string()],
        "facts must be re-derived under the swapped-in OVERRIDE_TYPE",
    );
    let epoch_type = meta(&db_path, "query_set_epoch").unwrap();
    assert_ne!(
        epoch_pkg, epoch_type,
        "swapping the active blob must bump _meta.query_set_epoch",
    );
    assert_eq!(
        meta(&db_path, "query_source:go").as_deref(),
        Some(format!("arena:{hex_type}").as_str()),
        "provenance must track the swapped-in blob's hash",
    );
}
