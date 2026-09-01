//! Review-gate 4 of the identity ladder (bead `ley-line-open-17c271`):
//! **the surrogate key must be at least as edit-stable as both schemes it
//! replaced, on every row of the six-edit matrix.**
//!
//! The six edits are the ones measured in the 2026-08-27 design review
//! (session f2c94691's `stability.py`): append/prepend a same-kind sibling,
//! append a different-kind sibling, rename a local, reflow, add a comment.
//! The review measured the pre-v5 path at 100/100/100% on
//! rename/reflow/comment but 17% on append-same-kind (the `needs_suffix`
//! singleton→pair rename), and the (source, start, end, kind) 4-tuple at
//! 36/36/23% — which is why the 4-tuple was withdrawn and the file-local
//! surrogate chosen.
//!
//! Operationalization (documented, deliberate): a base node SURVIVES an
//! edit iff the edited parse holds the same `(nid, raw_kind)` — the address
//! still denotes a structurally-equivalent position. Spans are excluded
//! (they move on reflow, and the 4-tuple's fragility is exactly what was
//! withdrawn); derived display names are excluded (they shift with sibling
//! cohorts by design, for the surrogate exactly as they did for the path).
//! Path survival is the display-path set intersection — byte-identical to
//! the pre-v5 measurement because v5 renders the same `{kind}[_{k}]`
//! scheme. Tuple survival is the `(start, end, kind)` set intersection.
//!
//! The assertion is exact counting, per edit: `nid ≥ max(path, tuple)`.

use rusqlite::Connection;
use std::collections::HashSet;
use std::fs;
use tempfile::TempDir;

use leyline_cli_lib::cmd_parse;

const BASE: &str = "package main\n\nfunc Alpha(a int) int {\n\tb := a + 1\n\treturn b\n}\n";

fn edits() -> Vec<(&'static str, String)> {
    vec![
        (
            "append same-kind sibling",
            format!("{BASE}\nfunc Beta(c int) int {{\n\td := c + 2\n\treturn d\n}}\n"),
        ),
        (
            "prepend same-kind sibling",
            format!(
                "package main\n\nfunc Zeta(c int) int {{\n\treturn c\n}}\n\n{}",
                BASE.trim_start_matches("package main\n\n")
            ),
        ),
        (
            "append different-kind sibling",
            format!("{BASE}\ntype T struct{{ X int }}\n"),
        ),
        (
            "rename a local",
            BASE.replace("b :=", "bb :=")
                .replace("return b", "return bb"),
        ),
        (
            "reflow: blank line in body",
            BASE.replace("\tb := a + 1", "\n\tb := a + 1"),
        ),
        (
            "comment above the function",
            BASE.replace("func Alpha", "// Alpha adds one.\nfunc Alpha"),
        ),
    ]
}

struct Snapshot {
    nid_kind: HashSet<(i64, String)>,
    paths: HashSet<String>,
    tuples: HashSet<(i64, i64, String)>,
}

fn parse(src: &str) -> Snapshot {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("x.go"), src).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    let mut nid_kind = HashSet::new();
    let mut tuples = HashSet::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT a.nid, a.start_byte, a.end_byte, k.raw_kind \
                 FROM _ast a JOIN kinds k ON k.kind_id = a.kind_id",
            )
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let nid: i64 = row.get(0).unwrap();
            let sb: i64 = row.get(1).unwrap();
            let eb: i64 = row.get(2).unwrap();
            let kind: String = row.get(3).unwrap();
            nid_kind.insert((nid, kind.clone()));
            tuples.insert((sb, eb, kind));
        }
    }
    let mut paths = HashSet::new();
    {
        let mut stmt = conn
            .prepare("SELECT p.path FROM _ast a JOIN v_node_path p ON p.nid = a.nid")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            paths.insert(row.get::<_, String>(0).unwrap());
        }
    }
    Snapshot {
        nid_kind,
        paths,
        tuples,
    }
}

#[test]
fn surrogate_is_at_least_as_stable_as_path_and_tuple_on_every_edit() {
    let base = parse(BASE);
    assert!(
        base.nid_kind.len() > 8,
        "fixture must produce a real tree; got {} nodes",
        base.nid_kind.len()
    );
    assert_eq!(
        base.nid_kind.len(),
        base.paths.len(),
        "every _ast row must render exactly one display path"
    );

    for (label, edited_src) in edits() {
        let edited = parse(&edited_src);
        let nid_surv = base.nid_kind.intersection(&edited.nid_kind).count();
        let path_surv = base.paths.intersection(&edited.paths).count();
        let tuple_surv = base.tuples.intersection(&edited.tuples).count();
        assert!(
            nid_surv >= path_surv.max(tuple_surv),
            "{label}: surrogate survival ({nid_surv}/{total}) must be ≥ \
             max(path {path_surv}, tuple {tuple_surv}) — the re-key may not \
             be more fragile than what it replaced (review-gate 4, \
             ley-line-open-17c271)",
            total = base.nid_kind.len(),
        );
    }
}
