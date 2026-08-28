//! F4c — review-gate 5 of the identity ladder (bead `ley-line-open-17c271`):
//! **file-discovery order must not move a single identity or the Σ root.**
//!
//! ## Claim
//!
//! Two cold parses of the same tree whose file DISCOVERY order differs — the
//! walker returning entries in a different order, or a scoped work-list handed
//! over in a different order — produce identical node identities in the
//! projection and an identical Σ segment root.
//!
//! ## Why F4 cannot catch this
//!
//! Every F4 gate parses the same fixture through the same walk, so both runs
//! see the same discovery order. An identity scheme that depends on that order
//! — e.g. a `file_id` assigned by position in the UNSORTED work-list, feeding
//! `nid = (file_id << 24) | ordinal` — is deterministic per run and identical
//! across F4's runs, so F4 stays green while two developers' arenas of the
//! same tree disagree on every node's name.
//!
//! ## The seam under test
//!
//! `parse_into_conn` normalizes discovery order in exactly one place: the
//! work-list sort (`to_parse.sort_unstable_by` in `cmd_parse.rs`). Everything
//! downstream — insert order, capnp segment append order, and (post
//! projection-v5) `file_id` assignment — inherits determinism from that sort.
//! Falsified live: with the sort removed, the Σ-equality assertion below goes
//! red (the `ast.capnp` / `source.capnp` segment logs are appended in
//! work-list order, so the segment root moves with it).
//!
//! ## What this gate is NOT
//!
//! - Not a scoped-reparse locality gate — that is F4d (review-gate 6).
//! - Not a claim that the Σ root survives a reparse across EDITS; only that
//!   the same bytes discovered in a different order address the same root.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Relative paths of the fixture files, in creation order. Nested dirs and
/// names chosen so lexicographic order differs from both creation order and
/// the reversed order handed to the shuffled parse.
const FIXTURE_FILES: &[&str] = &[
    "zeta/z.go",
    "alpha/m.go",
    "top.go",
    "alpha/beta/a.go",
    "alpha/beta/b.go",
    "mid.go",
];

fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    for (i, rel) in FIXTURE_FILES.iter().enumerate() {
        let path = td.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Same-package files with distinct bodies AND distinct node COUNTS
        // (i extra one-liner funcs per file). Distinct counts matter: with
        // equal per-file shapes, a permuted file_id assignment yields the
        // SAME nid multiset — the interning/Σ checks would still catch it,
        // but the nid comparison itself would be vacuous.
        let pkg = Path::new(rel)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("main");
        let extra: String = (0..i)
            .map(|k| format!("\nfunc Pad{i}x{k}() int {{\n\treturn {k}\n}}\n"))
            .collect();
        fs::write(
            &path,
            format!(
                "package {pkg}\n\n\
                 func Fn{i}A(x int) int {{\n\treturn x + {i}\n}}\n\n\
                 func Fn{i}B(s string) string {{\n\tif s == \"\" {{\n\t\treturn \"e{i}\"\n\t}}\n\treturn s\n}}\n{extra}"
            ),
        )
        .unwrap();
    }
    td
}

/// One cold parse into a fresh FILE-BACKED arena (the `:memory:` path skips
/// the capnp segment dual-write and the Σ head pass entirely).
///
/// Returns the projection's identity snapshot and the Σ root:
/// - `_ast` identities as `(node_id, source_id)` — post projection-v5 this
///   column pair changes type, not meaning; the assertion survives the re-key.
/// - `nodes` identities (files, dirs, and AST rows share this namespace).
/// - `Head.rootHash` from the sibling `head.capnp`.
fn parse_cold(src: &Path, scope: Option<&[String]>) -> Snapshot {
    let out = TempDir::new().unwrap();
    let db_path = out.path().join("arena.db");
    let conn = Connection::open(&db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, src, Some("go"), scope).unwrap();

    // projection-v5: the nid IS the identity (file in the high bits), and
    // the interning tables are part of it — a shuffled discovery order that
    // permuted file_id/dir_id/name_id assignment shows up in any of these.
    let ast = {
        let mut stmt = conn.prepare("SELECT nid FROM _ast ORDER BY nid").unwrap();
        let rows: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    let nodes = {
        let mut stmt = conn
            .prepare(
                "SELECT nid || ':' || COALESCE(parent_nid, '') || ':' || \
                 COALESCE(name_id, '') || ':' || COALESCE(kind_id, '') || ':' || ord \
                 FROM nodes ORDER BY nid",
            )
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    drop(conn);

    Snapshot {
        ast,
        nodes,
        sigma_root: read_sigma_root(&db_path),
    }
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    ast: Vec<i64>,
    nodes: Vec<String>,
    sigma_root: [u8; 32],
}

// Hand-rolled so a mismatch prints counts and the roots, not thousands of
// identity tuples.
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("ast_rows", &self.ast.len())
            .field("nodes_rows", &self.nodes.len())
            .field("sigma_root", &hex::encode(self.sigma_root))
            .finish()
    }
}

/// Read `Head.rootHash` from the arena's sibling `head.capnp`.
fn read_sigma_root(db_path: &Path) -> [u8; 32] {
    use leyline_schema_capnp::head_capnp::head;
    let head_path = db_path.with_extension("head.capnp");
    let bytes = fs::read(&head_path).expect("head.capnp must be written for a file-backed parse");
    let mut slice: &[u8] = &bytes;
    let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
        .expect("head.capnp must decode");
    let h: head::Reader = msg.get_root().expect("head root");
    let root: [u8; 32] = h
        .get_root_hash()
        .expect("rootHash present")
        .get_bytes()
        .expect("rootHash bytes")
        .try_into()
        .expect("rootHash must be exactly 32 bytes");
    root
}

#[test]
fn f4c_shuffled_discovery_produces_identical_identities_and_sigma_root() {
    let td = fixture_repo();

    // Three discovery orders for the SAME tree:
    // - the full-tree walk (whatever order the filesystem yields),
    // - an explicit work-list in lexicographic order,
    // - the same work-list reversed.
    // The scoped path takes the caller's list verbatim (`cmd_parse.rs`
    // scope handling), so the reversed list IS a shuffled discovery.
    let mut sorted: Vec<String> = FIXTURE_FILES.iter().map(|s| s.to_string()).collect();
    sorted.sort();
    let reversed: Vec<String> = sorted.iter().rev().cloned().collect();

    let walk = parse_cold(td.path(), None);
    let scope_sorted = parse_cold(td.path(), Some(&sorted));
    let scope_reversed = parse_cold(td.path(), Some(&reversed));

    // Vacuity guards: the fixture must actually produce a corpus and a root,
    // or the equalities below compare nothing.
    assert!(
        walk.ast.len() > 50,
        "fixture must produce >50 _ast rows; got {}",
        walk.ast.len()
    );
    assert!(
        walk.nodes.len() > 50,
        "fixture must produce >50 nodes rows; got {}",
        walk.nodes.len()
    );
    assert_ne!(
        walk.sigma_root, [0u8; 32],
        "Σ root must be non-zero for a real parse"
    );

    assert_eq!(
        walk, scope_sorted,
        "a scoped cold parse of the full file list must produce the same \
         identities and Σ root as the walk — discovery mechanism must not \
         leak into identity (review-gate 5, ley-line-open-17c271)"
    );
    assert_eq!(
        scope_sorted, scope_reversed,
        "reversing the discovery order must not move a single identity or \
         the Σ root. If this is red, some assignment downstream of discovery \
         (file_id, nid, segment append order) depends on work-list order \
         instead of being normalized by the sort (review-gate 5, \
         ley-line-open-17c271)"
    );
}
