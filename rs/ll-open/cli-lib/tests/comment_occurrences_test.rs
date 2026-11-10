//! Comment occurrences (bead `ley-line-open-aa1634`).
//!
//! tree-sitter marks comments as `extra` nodes: they can appear anywhere,
//! so the grammar attaches them to whichever node is open where they
//! occur. Before extraction epoch 7 the parse walk skipped every `extra`
//! child, so `_ast` held no comment and doc comments were unavailable to
//! consumers.
//!
//! The model under test is OCCURRENCE-ONLY:
//!
//! - every named `extra` whose grammar kind names a comment (`comment`,
//!   `line_comment`, `block_comment`, ...) is an `_ast` occurrence (and a
//!   `nodes` row) with its grammar kind and span, in source order, under
//!   the parent tree-sitter attached it to;
//! - a comment occurrence is terminal: its `content_id` names the leaf
//!   `node_content` row `hash_leaf(kind, comment text)`, so `token` holds
//!   the comment's text;
//! - comments stay OUT of every other node's `node_hash` fold, so a
//!   comment-only edit leaves every code `node_hash` unchanged, and the
//!   rest of the projection is exactly what it was.
//!
//! Every fixture runs through `parse_into_conn` end-to-end.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;

fn cold_parse(src_dir: &Path, lang: Option<&str>) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, lang, None)
        .expect("parse_into_conn must succeed");
    conn
}

fn parse_one(rel: &str, src: &str) -> Connection {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(rel), src).unwrap();
    cold_parse(dir.path(), None)
}

fn file_range(conn: &Connection, rel: &str) -> (i64, i64) {
    let file_id = leyline_ts::schema::lookup_file_id(conn, rel)
        .expect("file_id lookup")
        .unwrap_or_else(|| panic!("{rel} was never interned"));
    let (lo, hi) = leyline_ts::schema::file_nid_range(file_id);
    (lo.get(), hi.get())
}

/// One `_ast` occurrence as a consumer reads it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Occ {
    raw_kind: String,
    start_byte: i64,
    end_byte: i64,
    start: (i64, i64),
    end: (i64, i64),
    /// The grammar kind of the `nodes` parent's occurrence.
    parent_kind: Option<String>,
    node_hash: String,
    /// `node_content.token` (leaves only).
    token: Option<String>,
    /// `nodes.kind` (0 leaf, 1 dir) and `nodes.record`.
    node_kind: i64,
    record: String,
}

/// Every `_ast` occurrence of `rel`, in nid (pre-order) order.
fn occurrences(conn: &Connection, rel: &str) -> Vec<Occ> {
    let (lo, hi) = file_range(conn, rel);
    conn.prepare(
        "SELECT k.raw_kind, a.start_byte, a.end_byte, a.start_row, a.start_col,
                a.end_row, a.end_col, pk.raw_kind, hex(c.node_hash), c.token,
                n.kind, n.record
           FROM _ast a
           JOIN kinds k ON k.kind_id = a.kind_id
           JOIN node_content c ON c.content_id = a.content_id
           JOIN nodes n ON n.nid = a.nid
           LEFT JOIN _ast pa ON pa.nid = n.parent_nid
           LEFT JOIN kinds pk ON pk.kind_id = pa.kind_id
          WHERE a.nid BETWEEN ?1 AND ?2
          ORDER BY a.nid",
    )
    .unwrap()
    .query_map([lo, hi], |r| {
        Ok(Occ {
            raw_kind: r.get(0)?,
            start_byte: r.get(1)?,
            end_byte: r.get(2)?,
            start: (r.get(3)?, r.get(4)?),
            end: (r.get(5)?, r.get(6)?),
            parent_kind: r.get(7)?,
            node_hash: r.get(8)?,
            token: r.get(9)?,
            node_kind: r.get(10)?,
            record: r.get(11)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn is_comment_kind(kind: &str) -> bool {
    kind.contains("comment")
}

/// `(grammar kind, text, parent grammar kind)` of every comment occurrence,
/// in source order — with the span, `node_content.token` and `nodes.record`
/// all checked against the source bytes.
fn comments(conn: &Connection, rel: &str, src: &str) -> Vec<(String, String, String)> {
    occurrences(conn, rel)
        .into_iter()
        .filter(|o| is_comment_kind(&o.raw_kind))
        .map(|o| {
            let text = &src[o.start_byte as usize..o.end_byte as usize];
            assert_eq!(
                o.token.as_deref(),
                Some(text),
                "a comment occurrence names the leaf content row holding its text"
            );
            assert_eq!(
                (o.node_kind, o.record.as_str()),
                (0, text),
                "a comment's `nodes` row is a leaf carrying its text"
            );
            (
                o.raw_kind,
                text.to_string(),
                o.parent_kind.expect("a comment has a parent occurrence"),
            )
        })
        .collect()
}

fn c(kind: &str, text: &str, parent: &str) -> (String, String, String) {
    (kind.into(), text.into(), parent.into())
}

const GO: &str = "\
// Package p is documented.
package p

/* block */

// Add adds.
// Second line.
func Add(a, b int) int {
\t// inside
\treturn a + b /* trailing */
}

func E() {
\t// only
}
";

const RUST: &str = "\
//! Crate doc.

/// Outer doc.
fn add(a: i32) -> i32 {
    // line
    a /* block */
}

/** Block doc. */
struct S;
";

const PYTHON: &str = "\
# lead
def add(a, b):
    \"\"\"Docstring.\"\"\"
    # inside
    return a + \\
        b  # trailing
";

const TYPESCRIPT: &str = "\
/** Adds. */
export function add(a: number): number {
  // line
  return a; /* block */
}
";

const JAVA: &str = "\
/** Doc. */
class A {
  // line
  /* block */
  int f() { return 1; }
}
";

const C_SRC: &str = "\
/* block */
// line
int f(void) { return 1; }
";

#[test]
fn go_line_block_and_doc_comments_are_ast_occurrences() {
    let conn = parse_one("p.go", GO);
    assert_eq!(
        comments(&conn, "p.go", GO),
        vec![
            c("comment", "// Package p is documented.", "source_file"),
            c("comment", "/* block */", "source_file"),
            c("comment", "// Add adds.", "source_file"),
            c("comment", "// Second line.", "source_file"),
            c("comment", "// inside", "block"),
            c("comment", "/* trailing */", "statement_list"),
            c("comment", "// only", "block"),
        ]
    );
    // Span columns are the grammar's: the doc comment above `Add` is row 5.
    let add_doc = occurrences(&conn, "p.go")
        .into_iter()
        .find(|o| o.token.as_deref() == Some("// Add adds."))
        .unwrap();
    assert_eq!((add_doc.start, add_doc.end), ((5, 0), (5, 12)));
}

#[test]
fn rust_line_block_and_doc_comments_are_ast_occurrences() {
    let conn = parse_one("lib.rs", RUST);
    // A Rust line comment's span includes its newline (the grammar's
    // `line_comment` token does); a doc comment is told apart by its
    // `///`, `//!`, `/**` or `/*!` opener.
    assert_eq!(
        comments(&conn, "lib.rs", RUST),
        vec![
            c("line_comment", "//! Crate doc.\n", "source_file"),
            c("line_comment", "/// Outer doc.\n", "source_file"),
            c("line_comment", "// line", "block"),
            c("block_comment", "/* block */", "block"),
            c("block_comment", "/** Block doc. */", "source_file"),
        ]
    );
    // The grammar's doc-comment children are inside the terminal occurrence,
    // not occurrences of their own.
    assert!(
        occurrences(&conn, "lib.rs")
            .iter()
            .all(|o| !o.raw_kind.contains("doc_comment")),
        "comment occurrences are terminal"
    );
}

#[test]
fn python_comments_are_ast_occurrences_and_line_continuations_are_not() {
    let conn = parse_one("m.py", PYTHON);
    assert_eq!(
        comments(&conn, "m.py", PYTHON),
        vec![
            c("comment", "# lead", "module"),
            c("comment", "# inside", "block"),
            c("comment", "# trailing", "block"),
        ]
    );
    let occs = occurrences(&conn, "m.py");
    // `line_continuation` is a Python extra too, but not a comment.
    assert!(occs.iter().all(|o| o.raw_kind != "line_continuation"));
    // The docstring was never an extra: it stays a string occurrence.
    assert!(occs.iter().any(
        |o| o.raw_kind == "string" && o.parent_kind.as_deref() == Some("expression_statement")
    ));
}

#[test]
fn typescript_line_block_and_doc_comments_are_ast_occurrences() {
    let conn = parse_one("a.ts", TYPESCRIPT);
    assert_eq!(
        comments(&conn, "a.ts", TYPESCRIPT),
        vec![
            c("comment", "/** Adds. */", "program"),
            c("comment", "// line", "statement_block"),
            c("comment", "/* block */", "statement_block"),
        ]
    );
}

#[test]
fn java_and_c_comments_are_ast_occurrences() {
    let conn = parse_one("A.java", JAVA);
    assert_eq!(
        comments(&conn, "A.java", JAVA),
        vec![
            c("block_comment", "/** Doc. */", "program"),
            c("line_comment", "// line", "class_body"),
            c("block_comment", "/* block */", "class_body"),
        ]
    );
    let conn = parse_one("f.c", C_SRC);
    assert_eq!(
        comments(&conn, "f.c", C_SRC),
        vec![
            c("comment", "/* block */", "translation_unit"),
            c("comment", "// line", "translation_unit"),
        ]
    );
}

/// The `(grammar kind, node_hash)` of every NON-comment occurrence, in
/// pre-order: the code's identity, nids aside.
fn code_hashes(conn: &Connection, rel: &str) -> Vec<(String, String)> {
    occurrences(conn, rel)
        .into_iter()
        .filter(|o| !is_comment_kind(&o.raw_kind))
        .map(|o| (o.raw_kind, o.node_hash))
        .collect()
}

fn root_hash(conn: &Connection, rel: &str) -> String {
    occurrences(conn, rel).remove(0).node_hash
}

/// The Go fixture with every comment edited, one removed, one added (inside
/// an expression) — and the same code with no comment at all.
const GO_COMMENTS_EDITED: &str = "\
// Package p: a different sentence.
package p

// Add adds two ints.
func Add(a, b int) int {
\t// INSIDE, rewritten
\treturn a /* new */ + b
}

func E() {
\t/* only, now a block */
}
";

const GO_NO_COMMENTS: &str = "\
package p

func Add(a, b int) int {
\treturn a + b
}

func E() {
}
";

/// The pinned root `node_hash` of the Go fixture: the merkle-ast-v3 address
/// of its comment-free code, the same value the epoch-6 binary (which
/// skipped every extra) produced for it.
const GO_ROOT_HASH: &str = "9CE6853BA1618E3FB853A21047B63BEF1ACF9EC0FE8187C8D7A6C6963A0FB2FA";

const RUST_COMMENTS_EDITED: &str = "\
/// Outer doc, reworded.
fn add(a: i32) -> i32 {
    a
}

struct S;
";

const RUST_ROOT_HASH: &str = "4406E6257A15DB9CD6EEACA16D9922C3AB546165A3D7BDA6207694EAB571C4AE";

#[test]
fn a_comment_only_edit_changes_no_node_hash() {
    let before = parse_one("p.go", GO);
    let edited = parse_one("p.go", GO_COMMENTS_EDITED);
    let bare = parse_one("p.go", GO_NO_COMMENTS);
    assert_eq!(code_hashes(&before, "p.go"), code_hashes(&edited, "p.go"));
    assert_eq!(code_hashes(&before, "p.go"), code_hashes(&bare, "p.go"));
    assert_eq!(root_hash(&before, "p.go"), GO_ROOT_HASH);
    assert_eq!(root_hash(&bare, "p.go"), GO_ROOT_HASH);

    let before = parse_one("lib.rs", RUST);
    let edited = parse_one("lib.rs", RUST_COMMENTS_EDITED);
    assert_eq!(
        code_hashes(&before, "lib.rs"),
        code_hashes(&edited, "lib.rs")
    );
    assert_eq!(root_hash(&before, "lib.rs"), RUST_ROOT_HASH);

    // The comment's OWN address follows its text: an edited comment is a
    // different leaf, an unchanged one is the same leaf.
    let comment_hash = |conn: &Connection, rel: &str, text: &str| -> String {
        occurrences(conn, rel)
            .into_iter()
            .find(|o| o.token.as_deref() == Some(text))
            .unwrap_or_else(|| panic!("no comment {text:?}"))
            .node_hash
    };
    let go = parse_one("p.go", GO);
    let go_edited = parse_one("p.go", GO_COMMENTS_EDITED);
    assert_ne!(
        comment_hash(&go, "p.go", "// Add adds."),
        comment_hash(&go_edited, "p.go", "// Add adds two ints.")
    );
    let other = parse_one("q.go", "package q\n\n// Add adds.\nfunc Q() {}\n");
    assert_eq!(
        comment_hash(&go, "p.go", "// Add adds."),
        comment_hash(&other, "q.go", "// Add adds."),
        "a comment's address is a function of its kind and text alone"
    );
}

/// The `(nid, grammar kind)` of every non-comment occurrence.
fn code_nids(conn: &Connection, rel: &str) -> Vec<(i64, String)> {
    let (lo, hi) = file_range(conn, rel);
    conn.prepare(
        "SELECT a.nid, k.raw_kind FROM _ast a JOIN kinds k ON k.kind_id = a.kind_id
          WHERE a.nid BETWEEN ?1 AND ?2 AND k.raw_kind NOT LIKE '%comment%'
          ORDER BY a.nid",
    )
    .unwrap()
    .query_map([lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Comments take the ordinals after every code occurrence, so adding,
/// editing or removing one moves no code `nid` (review-gate 4,
/// `stability_matrix_gate.rs`), while `nodes.ord` keeps source order among
/// code and comment siblings alike.
#[test]
fn a_comment_only_edit_moves_no_code_nid() {
    let with = parse_one("p.go", GO);
    let edited = parse_one("p.go", GO_COMMENTS_EDITED);
    let bare = parse_one("p.go", GO_NO_COMMENTS);
    assert_eq!(code_nids(&with, "p.go"), code_nids(&bare, "p.go"));
    assert_eq!(code_nids(&edited, "p.go"), code_nids(&bare, "p.go"));
    let (lo, _) = file_range(&with, "p.go");
    let max_code = code_nids(&with, "p.go").last().unwrap().0;
    let min_comment: i64 = with
        .query_row(
            "SELECT MIN(a.nid) FROM _ast a JOIN kinds k ON k.kind_id = a.kind_id
              WHERE k.raw_kind = 'comment' AND a.nid >= ?1",
            [lo],
            |r| r.get(0),
        )
        .unwrap();
    assert!(min_comment > max_code, "comments come after the code");
    // Source order among the root's children: comment, package, comment,
    // comment, comment, Add, E.
    let root_children: Vec<String> = with
        .prepare(
            "SELECT k.raw_kind FROM nodes n JOIN _ast a ON a.nid = n.nid
               JOIN kinds k ON k.kind_id = a.kind_id
              WHERE n.parent_nid = ?1 ORDER BY n.ord",
        )
        .unwrap()
        .query_map([lo], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        root_children,
        [
            "comment",
            "package_clause",
            "comment",
            "comment",
            "comment",
            "function_declaration",
            "function_declaration"
        ]
    );
}

#[test]
fn a_code_edit_in_a_commented_file_still_changes_the_expected_hashes() {
    let before = parse_one("p.go", GO);
    let after = parse_one("p.go", &GO.replace("return a + b", "return a - b"));
    let hash_of = |conn: &Connection, kind: &str, idx: usize| -> String {
        occurrences(conn, "p.go")
            .into_iter()
            .filter(|o| o.raw_kind == kind)
            .nth(idx)
            .unwrap()
            .node_hash
    };
    assert_ne!(root_hash(&before, "p.go"), root_hash(&after, "p.go"));
    // `Add` changed; `E` did not.
    assert_ne!(
        hash_of(&before, "function_declaration", 0),
        hash_of(&after, "function_declaration", 0)
    );
    assert_eq!(
        hash_of(&before, "function_declaration", 1),
        hash_of(&after, "function_declaration", 1)
    );
    assert_ne!(
        hash_of(&before, "binary_expression", 0),
        hash_of(&after, "binary_expression", 0)
    );
    // Every comment occurrence is unchanged.
    let comment_rows = |conn: &Connection| -> Vec<Occ> {
        occurrences(conn, "p.go")
            .into_iter()
            .filter(|o| is_comment_kind(&o.raw_kind))
            .collect()
    };
    assert_eq!(comment_rows(&before), comment_rows(&after));
}

/// `src` with every comment occurrence's bytes replaced by spaces, newlines
/// kept: the same spans, the same parse, no comments.
fn blank_comments(conn: &Connection, rel: &str, src: &str) -> String {
    let mut bytes = src.as_bytes().to_vec();
    for o in occurrences(conn, rel) {
        if is_comment_kind(&o.raw_kind) {
            for b in &mut bytes[o.start_byte as usize..o.end_byte as usize] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

/// Everything a file's parse writes, rendered without arena-local ids, the
/// comment occurrences left out: `_ast` (kind, parent kind, hash, and a
/// terminal's span), the def and ref rows (token, qualifier, kind, hash),
/// `_imports`, and the edge rows.
fn projection_without_comments(conn: &Connection, rel: &str) -> Vec<String> {
    let (lo, hi) = file_range(conn, rel);
    let mut out: Vec<String> = occurrences(conn, rel)
        .into_iter()
        .filter(|o| !is_comment_kind(&o.raw_kind))
        .map(|o| {
            // Two things the oracle cannot compare, both tree-sitter's and
            // both the same before this change, since the tree is: an inner
            // node's span stretches over a comment at its start or end (the
            // root over a leading comment, a Python `block` over a trailing
            // one) but not over whitespace, so only a terminal's span is
            // compared (a def or ref row's span is its anchor's, so it is
            // left out below); and `nodes.kind` counted a comment child (it
            // was `is_named`) before the comment had a row, so `{ // only }`
            // is a dir in the source and a leaf once blanked
            // (`comment_parents_are_dirs_and_leaves_have_no_rows` pins it).
            if o.token.is_some() {
                format!(
                    "_ast {} {}..{} {:?}..{:?} parent={:?} {}",
                    o.raw_kind,
                    o.start_byte,
                    o.end_byte,
                    o.start,
                    o.end,
                    o.parent_kind,
                    o.node_hash,
                )
            } else {
                format!(
                    "_ast {} parent={:?} {}",
                    o.raw_kind, o.parent_kind, o.node_hash
                )
            }
        })
        .collect();
    for table in ["v_node_defs_hash", "v_node_refs_hash"] {
        let qualifier = if table == "v_node_refs_hash" {
            "r.qualifier"
        } else {
            "NULL"
        };
        let mut rows: Vec<String> = conn
            .prepare(&format!(
                "SELECT r.token, {qualifier}, k.lang, k.raw_kind,
                        hex(r.node_hash), r.container_nid IS NULL
                   FROM {table} r LEFT JOIN kinds k ON k.kind_id = r.kind_id
                  WHERE r.nid BETWEEN ?1 AND ?2"
            ))
            .unwrap()
            .query_map([lo, hi], |r| {
                Ok(format!(
                    "{table} {:?} {:?} {:?} {:?} {:?} top={:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.sort();
        out.extend(rows);
    }
    let mut imports: Vec<String> = conn
        .prepare("SELECT * FROM _imports")
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "_imports {:?} {:?}",
                r.get::<_, rusqlite::types::Value>(0)?,
                r.get::<_, rusqlite::types::Value>(1)?
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    imports.sort();
    out.extend(imports);
    let mut edges: Vec<String> = conn
        .prepare(
            "SELECT hex(parent_hash), ordinal, hex(child_hash), field FROM node_child
              ORDER BY 1, 2",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "edge {} {} {} {:?}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out.append(&mut edges);
    out
}

fn content_hashes(conn: &Connection) -> BTreeSet<String> {
    conn.prepare("SELECT hex(node_hash) FROM node_content")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The projection with comments equals the projection of the same file with
/// its comments blanked to whitespace, plus the comment occurrences and their
/// leaf content rows — nothing else moves.
#[test]
fn comments_add_occurrences_and_change_nothing_else() {
    for (rel, src) in [
        ("p.go", GO),
        ("lib.rs", RUST),
        ("m.py", PYTHON),
        ("a.ts", TYPESCRIPT),
        ("A.java", JAVA),
        ("f.c", C_SRC),
    ] {
        let with = parse_one(rel, src);
        let blanked_src = blank_comments(&with, rel, src);
        let without = parse_one(rel, &blanked_src);
        assert!(
            occurrences(&with, rel)
                .iter()
                .any(|o| is_comment_kind(&o.raw_kind)),
            "{rel}: the fixture has comments"
        );
        assert!(
            occurrences(&without, rel)
                .iter()
                .all(|o| !is_comment_kind(&o.raw_kind)),
            "{rel}: the blanked fixture has none"
        );
        let mut expected = projection_without_comments(&without, rel);
        if rel == "lib.rs" {
            // Pre-existing extraction gap, not this change (bead
            // `ley-line-open-0ed720`): the epoch-6 Rust tail-expression
            // value ref anchors on the block's last NAMED child, and the
            // `/* block */` after `a` is one, so the source with the comment
            // has no `a` ref. Remove this when 0ed720 lands.
            let tail = expected
                .iter()
                .position(|r| {
                    r.starts_with("v_node_refs_hash \"a\" None Some(\"rust\") Some(\"block\")")
                })
                .expect("the blanked source emits the tail value ref");
            expected.remove(tail);
        }
        assert_eq!(
            projection_without_comments(&with, rel),
            expected,
            "{rel}: comments must add occurrences and nothing else"
        );
        let comment_leaves: BTreeSet<String> = occurrences(&with, rel)
            .into_iter()
            .filter(|o| is_comment_kind(&o.raw_kind))
            .map(|o| o.node_hash)
            .collect();
        let expected: BTreeSet<String> = content_hashes(&without)
            .union(&comment_leaves)
            .cloned()
            .collect();
        assert_eq!(
            content_hashes(&with),
            expected,
            "{rel}: the only new content rows are the comment leaves"
        );
    }
}

/// A comment is a leaf `nodes` row under a dir: every comment's parent has
/// `nodes.kind = 1`, and no `nodes.kind = 0` row has a child.
#[test]
fn comment_parents_are_dirs_and_leaves_have_no_rows() {
    for (rel, src) in [("p.go", GO), ("lib.rs", RUST), ("m.py", PYTHON)] {
        let conn = parse_one(rel, src);
        let (lo, hi) = file_range(&conn, rel);
        let leaf_parents: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes c JOIN nodes p ON p.nid = c.parent_nid
                  WHERE c.nid BETWEEN ?1 AND ?2 AND p.kind = 0",
                [lo, hi],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leaf_parents, 0, "{rel}: a leaf `nodes` row has a child");
    }
}

/// Doc comments: the run of comment occurrences immediately before a def's
/// occurrence among its siblings, with no blank line inside the run or
/// between the run and the def. The recipe the mache adoption note gives,
/// run as written. A comment ending at column 0 (a Rust line comment, whose
/// span takes its newline) ends on the row before its `end_row`.
#[test]
fn a_defs_doc_comment_is_its_preceding_comment_run() {
    const PRECEDING: &str = "
        WITH RECURSIVE run(nid, parent_nid, ord, start_row, depth) AS (
            SELECT n.nid, n.parent_nid, n.ord, a.start_row, 0
              FROM node_defs d
              JOIN nodes n ON n.nid = d.nid
              JOIN _ast a ON a.nid = d.nid
             WHERE d.token = ?1
            UNION ALL
            SELECT s.nid, s.parent_nid, s.ord, sa.start_row, run.depth + 1
              FROM run
              JOIN nodes s ON s.parent_nid = run.parent_nid AND s.ord = run.ord - 1
              JOIN _ast sa ON sa.nid = s.nid
              JOIN kinds k ON k.kind_id = sa.kind_id
             WHERE k.raw_kind IN ('comment', 'line_comment', 'block_comment')
               AND sa.end_row - (sa.end_col = 0) >= run.start_row - 1
        )
        SELECT k.raw_kind, cc.token
          FROM run
          JOIN _ast a ON a.nid = run.nid
          JOIN kinds k ON k.kind_id = a.kind_id
          JOIN node_content cc ON cc.content_id = a.content_id
         WHERE run.depth > 0
         ORDER BY run.ord";
    let doc = |conn: &Connection, token: &str| -> Vec<(String, String)> {
        conn.prepare(PRECEDING)
            .unwrap()
            .query_map([token], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let go = parse_one("p.go", GO);
    assert_eq!(
        doc(&go, "Add"),
        vec![
            ("comment".to_string(), "// Add adds.".to_string()),
            ("comment".to_string(), "// Second line.".to_string()),
        ]
    );
    assert_eq!(doc(&go, "E"), vec![]);
    let rust = parse_one("lib.rs", RUST);
    assert_eq!(
        doc(&rust, "add"),
        vec![("line_comment".to_string(), "/// Outer doc.\n".to_string())],
        "the blank line ends the run before the inner `//!` doc"
    );
    assert_eq!(
        doc(&rust, "S"),
        vec![("block_comment".to_string(), "/** Block doc. */".to_string())]
    );
}

/// Injection still works with comments around the literal (cc5c55): the
/// injected def keeps its own language and grammar kind and its nid past
/// the host's `_ast` count, with no `_ast` row.
#[test]
fn injection_is_unaffected_by_comment_occurrences() {
    const Q: &str = "package q\n\nimport \"database/sql\"\n\n\
        // setup creates the table.\n\
        func setup(db *sql.DB) {\n\t/* the schema */\n\tdb.Exec(`CREATE TABLE users (id INTEGER)`) // inline\n}\n";
    let conn = parse_one("q.go", Q);
    assert_eq!(
        comments(&conn, "q.go", Q).len(),
        3,
        "the three comments are occurrences"
    );
    let (nid, lang, kind): (i64, String, String) = conn
        .query_row(
            "SELECT d.nid, k.lang, k.raw_kind FROM node_defs d JOIN kinds k ON k.kind_id = d.kind_id
              WHERE d.token = 'users'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((lang.as_str(), kind.as_str()), ("sql", "create_table"));
    let (lo, _) = file_range(&conn, "q.go");
    let ast_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _ast WHERE nid BETWEEN ?1 AND ?1 + 16777215",
            [lo],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        nid - lo >= ast_count,
        "injected ordinal past the host's `_ast` count"
    );
    let has_ast: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast WHERE nid = ?1", [nid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(has_ast, 0);
}
