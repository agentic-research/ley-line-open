//! ADR-0028 Phase 1 dual-store — source_blobs F-gates + adversarial tests.
//!
//! Bead `ley-line-open-9e4416`. Pins the Phase 1 contract: every parse
//! populates BOTH the row-projected `_source` schema AND the content-
//! addressed source-blob store (`source_blobs`), and every `_source` row's
//! `content_hash` resolves to a `source_blobs` row whose `blob_bytes` is
//! byte-identical to the file on disk.
//!
//! F1s (round-trip integrity) is the load-bearing gate for Phase 1
//! (ADR-0028 §4.F1s + §7). If it ever fails, the source blob store cannot
//! serve the same queries as reading through `_source` — the design bet is
//! broken and Phase 2 must not begin.
//!
//! F-git is the load-bearing SUBSTRATE gate: it proves BLAKE3-source-blobs
//! are byte-identity-compatible with what `git cat-file blob` returns. If
//! F-git ever fails, the unified-CAS composition claim (ADR-0028 §3.1) is
//! falsified and the future git-ingest ADR cannot proceed.

use std::fs;
use std::process::Command;

use blake3;
use leyline_cli_lib::cmd_parse::parse_into_conn;
use rusqlite::{Connection, params};
use tempfile::TempDir;

// ── Fixtures ──────────────────────────────────────────────────────────────

/// Five-file Go fixture with distinct contents. Same shape as the ADR-0026
/// F1 fixture so the two gates share a mental model. AST rows total ~200+;
/// _source rows total 5.
fn create_go_fixture() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    fs::write(
        dir.path().join("main.go"),
        b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(add(1, 2))\n}\n",
    )
    .expect("write main.go");
    fs::write(
        dir.path().join("util.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n\nfunc sub(a, b int) int {\n\treturn a - b\n}\n",
    )
    .expect("write util.go");
    fs::write(
        dir.path().join("types.go"),
        b"package main\n\ntype Point struct {\n\tX int\n\tY int\n}\n\ntype Vec struct {\n\tDX int\n\tDY int\n}\n",
    )
    .expect("write types.go");
    fs::write(
        dir.path().join("iface.go"),
        b"package main\n\ntype Adder interface {\n\tAdd(a, b int) int\n}\n",
    )
    .expect("write iface.go");
    fs::write(
        dir.path().join("consts.go"),
        b"package main\n\nconst Pi = 3\n\nvar Origin = Point{X: 0, Y: 0}\n",
    )
    .expect("write consts.go");
    dir
}

// ── Basic dual-store plumbing ─────────────────────────────────────────────

/// Both schemas MUST be populated after a fresh parse.
#[test]
fn dual_store_populates_both_schemas() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    let r = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    assert_eq!(r.parsed, 5, "all five fixture files must parse");

    let source_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source", [], |row| row.get(0))
        .unwrap();
    let blob_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_blobs", [], |row| row.get(0))
        .unwrap();

    assert_eq!(source_rows, 5, "row-projected _source must be populated");
    assert!(
        blob_rows > 0,
        "source_blobs must be populated (Phase 1 dual-store)",
    );
    // Every `_source.content_hash` must be non-null and match a source_blob.
    let unresolved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _source s \
             LEFT JOIN source_blobs b ON b.blob_hash = s.content_hash \
             WHERE b.blob_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        unresolved, 0,
        "every _source.content_hash MUST resolve in source_blobs",
    );
}

/// Every `blob_hash` in `source_blobs` must equal BLAKE3 of its `blob_bytes`.
/// Content-addressing is the whole point of the store; a producer that lets
/// the two drift silently corrupts the F4s / F5s dedup claims.
#[test]
fn blob_hash_matches_blake3_of_bytes() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let mut stmt = conn
        .prepare("SELECT blob_hash, blob_bytes FROM source_blobs")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut checked = 0;
    while let Some(row) = rows.next().unwrap() {
        let stored_hash: Vec<u8> = row.get(0).unwrap();
        let bytes: Vec<u8> = row.get(1).unwrap();
        assert_eq!(stored_hash.len(), 32, "blob_hash must be a 32-byte BLAKE3");
        let recomputed = *blake3::hash(&bytes).as_bytes();
        assert_eq!(
            stored_hash.as_slice(),
            &recomputed[..],
            "blob_hash MUST equal BLAKE3(blob_bytes) — content-address invariant",
        );
        checked += 1;
    }
    assert!(checked > 0, "at least one blob must exist to verify");
}

/// Schema shape pin: `byte_len` is a stored generated column derived from
/// `length(blob_bytes)`. A refactor that dropped GENERATED or renamed the
/// expression silently changes storage semantics.
#[test]
fn byte_len_matches_blob_bytes_length() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let mismatches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM source_blobs WHERE byte_len != length(blob_bytes)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        mismatches, 0,
        "byte_len MUST equal length(blob_bytes) for every row",
    );
}

// ── F1s: round-trip integrity (ADR-0028 §4.F1s) ───────────────────────────

/// **F1s (ADR-0028 §4.F1s) — the load-bearing Phase 1 gate.**
///
/// For every `_source` row, look up `source_blobs[content_hash].blob_bytes`
/// and assert it byte-identical to the file at `_source.path` on disk.
///
/// This is the falsifier that ADR-0028 §7 says must run continuously during
/// Phase 1. If it fails, the source blob store cannot serve the same reads
/// as the direct-file path and Phase 2 is off the table.
#[test]
fn f1s_round_trip_integrity() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.path, s.content_hash, b.blob_bytes \
             FROM _source s JOIN source_blobs b ON b.blob_hash = s.content_hash",
        )
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut checked = 0usize;
    while let Some(row) = rows.next().unwrap() {
        let id: String = row.get(0).unwrap();
        let path: String = row.get(1).unwrap();
        let stored_hash: Vec<u8> = row.get(2).unwrap();
        let blob_bytes: Vec<u8> = row.get(3).unwrap();

        // Read the file straight off disk — the "primary" source we're
        // asserting parity against.
        let disk_bytes = fs::read(&path)
            .unwrap_or_else(|e| panic!("F1s: unable to read {path} for source_id={id}: {e}"));
        assert_eq!(
            blob_bytes, disk_bytes,
            "F1s: source_blobs[content_hash].blob_bytes MUST equal fs::read(_source.path) for source_id={id}",
        );
        // And the hash must be BLAKE3 of those bytes (content-address).
        let expect = *blake3::hash(&disk_bytes).as_bytes();
        assert_eq!(
            stored_hash.as_slice(),
            &expect[..],
            "F1s: content_hash MUST equal BLAKE3(fs::read(_source.path)) for source_id={id}",
        );
        checked += 1;
    }
    assert_eq!(checked, 5, "F1s must have verified all five _source rows");
}

// ── F4s: cross-generation dedup (ADR-0028 §4.F4s) ─────────────────────────

/// **F4s (ADR-0028 §4.F4s).** Reparsing an unchanged corpus must not add
/// rows to `source_blobs`. Content-addressing + `INSERT OR IGNORE` gives
/// this for free — the falsifier verifies the mechanic actually holds
/// end-to-end.
#[test]
fn f4s_cross_generation_dedup() {
    let src = create_go_fixture();

    // Persistent (file-backed) DB so schema + rows survive across the two
    // `parse_into_conn` calls that mimic reparse.
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("f4s.db");
    let count_after = |run: &str| -> i64 {
        let c = Connection::open(&db_path).unwrap();
        parse_into_conn(&c, src.path(), Some("go"), None)
            .unwrap_or_else(|e| panic!("F4s: parse {run} failed: {e:#}"));
        c.query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
            .unwrap()
    };

    let n1 = count_after("first");
    let n2 = count_after("second");
    assert_eq!(
        n1, n2,
        "F4s: reparse of unchanged corpus MUST NOT add source_blobs rows \
         (got {n1} → {n2})",
    );
}

// ── F5s: cross-file dedup on byte-identical content (ADR-0028 §4.F5s) ──────

/// **F5s (ADR-0028 §4.F5s).** N files with byte-identical content must
/// share ONE `source_blobs` row while every path still gets its own
/// `_source` row. Deliberately uses 5 files, per the bead spec (the ADR's
/// N=100 is the same pattern at scale — 5 is sufficient to falsify at CI
/// speed).
#[test]
fn f5s_cross_file_dedup_on_identical_content() {
    let dir = TempDir::new().unwrap();
    // Byte-identical across five paths.
    let content = b"package main\n\nfunc identical() {}\n";
    for name in ["a.go", "b.go", "c.go", "d.go", "e.go"] {
        fs::write(dir.path().join(name), content).unwrap();
    }
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();

    let source_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source", [], |r| r.get(0))
        .unwrap();
    let blob_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(source_rows, 5, "5 files → 5 _source rows (path index)");
    assert_eq!(
        blob_rows, 1,
        "F5s: 5 byte-identical files MUST share ONE source_blobs row",
    );

    // All five _source rows point at the same blob_hash.
    let distinct_hashes: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT content_hash) FROM _source",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        distinct_hashes, 1,
        "F5s: all 5 _source.content_hash values MUST be the same blob_hash",
    );

    // And the shared hash matches BLAKE3(content).
    let expect = *blake3::hash(content).as_bytes();
    let stored: Vec<u8> = conn
        .query_row("SELECT blob_hash FROM source_blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored.as_slice(), &expect[..]);
}

// ── F-rename: rename preserves dedup (ADR-0028 §4.F-rename) ───────────────

/// **F-rename (ADR-0028 §4.F-rename).** Moving a file preserves its
/// `content_hash` — the blob is unchanged, only the path index moves. Two
/// invariants: (1) the renamed row's content_hash matches the pre-rename
/// hash, (2) source_blobs row count doesn't grow across the rename.
#[test]
fn f_rename_preserves_dedup() {
    let src = TempDir::new().unwrap();
    let content = b"package main\n\nfunc renamed() {}\n";
    fs::write(src.path().join("a.go"), content).unwrap();

    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("f_rename.db");

    // Parse #1: file `a.go`.
    let expected_hash = *blake3::hash(content).as_bytes();
    let blob_count_1: i64;
    let hash_at_a: Vec<u8>;
    {
        let conn = Connection::open(&db_path).unwrap();
        parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
        blob_count_1 = conn
            .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
            .unwrap();
        hash_at_a = conn
            .query_row(
                "SELECT content_hash FROM _source WHERE id = 'a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash_at_a.as_slice(), &expected_hash[..]);
        assert_eq!(blob_count_1, 1);
    }

    // Rename a.go → b.go on disk, reparse.
    fs::rename(src.path().join("a.go"), src.path().join("b.go")).unwrap();
    {
        let conn = Connection::open(&db_path).unwrap();
        parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

        // The renamed row's content_hash must match the original hash.
        let hash_at_b: Vec<u8> = conn
            .query_row(
                "SELECT content_hash FROM _source WHERE id = 'b.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hash_at_b, hash_at_a,
            "F-rename: _source[b.go].content_hash MUST equal the pre-rename hash of a.go",
        );

        // The old row must be gone (delete-file sweep).
        let a_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _source WHERE id = 'a.go'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a_count, 0, "F-rename: stale _source[a.go] row must be gone");

        // Blob count unchanged (the blob is content-addressed, orphans are
        // acceptable per ADR-0028 §7 Phase 2/3 GC — but for a byte-identical
        // rename the same blob is reused via INSERT OR IGNORE, so no new
        // row appears either).
        let blob_count_2: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            blob_count_2, blob_count_1,
            "F-rename: source_blobs row count MUST NOT grow across a rename \
             (got {blob_count_1} → {blob_count_2})",
        );
    }
}

// ── F-git: git-compat proof (ADR-0028 §4.F-git) ───────────────────────────

/// **F-git (ADR-0028 §4.F-git) — LOAD-BEARING SUBSTRATE GATE.**
///
/// git blobs are content-addressed via `git hash-object` (SHA-1 today, but
/// the byte-identity of the git blob object payload is what matters). LLO's
/// substrate stores the same bytes under BLAKE3. This test proves the
/// two hash the SAME byte sequence: `git cat-file blob <sha>` returns the
/// file bytes, and BLAKE3 of those bytes must equal LLO's
/// `source_blobs.blob_hash` for the same file.
///
/// If this test fails, the unified-CAS composition claim (ADR-0028 §3) is
/// falsified — LLO's source blobs cannot compose with git-object ingest
/// under a shared BLAKE3 view. Per the bead's "if blocked" instruction:
/// this failure is substrate-level falsification, not a bug to work
/// around; it should stop Phase 1 and comment on bead 9e4416.
#[test]
fn f_git_git_blob_compat() {
    // Skip cleanly when git isn't available (CI runners without git
    // should not fail this suite — the test is a substrate compat proof,
    // not a hard-dep tax). We error loudly if git *is* present and the
    // hash doesn't match.
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("F-git: git not on PATH — skipping");
        return;
    }

    let src = TempDir::new().unwrap();
    let content = b"package main\n\nfunc gitcompat() {\n\treturn\n}\n";
    fs::write(src.path().join("main.go"), content).unwrap();

    // Minimal git init + commit so `git cat-file blob <sha>` can round-trip.
    let git = |args: &[&str]| -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(src.path())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).unwrap()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "main.go"]);
    git(&["commit", "-q", "-m", "seed"]);

    // Get the git blob SHA for main.go.
    let blob_sha = git(&["rev-parse", "HEAD:main.go"]).trim().to_string();
    assert!(!blob_sha.is_empty());

    // Extract the raw blob bytes via `git cat-file blob` — this is what
    // a future git-object ingest would read from `.git/objects`.
    let git_blob_bytes = {
        let out = Command::new("git")
            .args(["cat-file", "blob", &blob_sha])
            .current_dir(src.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    };

    // Sanity: the git blob bytes MUST match the file we wrote — this is
    // git's own byte-identity contract for blobs (unlike commits/trees,
    // blobs are the file content verbatim, no header).
    assert_eq!(
        git_blob_bytes,
        content.to_vec(),
        "F-git prerequisite: git blob bytes MUST equal the on-disk file",
    );

    let expected_hash = *blake3::hash(&git_blob_bytes).as_bytes();

    // Parse through LLO.
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let stored_hash: Vec<u8> = conn
        .query_row(
            "SELECT b.blob_hash FROM _source s \
             JOIN source_blobs b ON b.blob_hash = s.content_hash \
             WHERE s.id = 'main.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_hash.as_slice(),
        &expected_hash[..],
        "F-git: source_blobs.blob_hash MUST equal BLAKE3(`git cat-file blob {blob_sha}`). \
         Substrate is NOT git-blob compatible — see ADR-0028 §4.F-git kill-criterion.",
    );

    // Cross-check: the stored blob_bytes and git's blob bytes are byte-
    // identical too (F1s + F-git compose into a full round trip).
    let stored_bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.blob_bytes FROM source_blobs b \
             JOIN _source s ON s.content_hash = b.blob_hash \
             WHERE s.id = 'main.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_bytes, git_blob_bytes,
        "F-git: source_blobs.blob_bytes MUST equal `git cat-file blob` output",
    );
}

// ── Adversarial 1: transaction atomicity ──────────────────────────────────

/// Force a mid-transaction failure between a `source_blobs` insert and a
/// `_source` insert. Assert both writes roll back — no orphan blob left
/// behind, no orphan _source row. Exercises the SQLite atomicity contract
/// this Phase relies on (the parse-time loop wraps every write in a single
/// BEGIN/COMMIT — if the COMMIT never happens, nothing lands).
#[test]
fn adversarial_transaction_atomicity() {
    use leyline_ts::schema::{
        create_ast_tables, create_index_schema, create_pointer_store_tables, create_refs_tables,
        create_source_blobs_table,
    };

    let conn = Connection::open_in_memory().unwrap();
    create_ast_tables(&conn).unwrap();
    create_refs_tables(&conn).unwrap();
    create_index_schema(&conn).unwrap();
    create_pointer_store_tables(&conn).unwrap();
    create_source_blobs_table(&conn).unwrap();

    // Seed a conflicting _source row so the SECOND insert below fails on
    // PK conflict — the natural way to force a mid-transaction failure
    // without touching parse_into_conn's internals.
    conn.execute(
        "INSERT INTO _source (id, language, path, content_hash) \
         VALUES ('a.go', 'go', '/tmp/a.go', X'00')",
        [],
    )
    .unwrap();

    let blob_bytes = b"package a\n";
    let blob_hash = *blake3::hash(blob_bytes).as_bytes();

    // Pre-condition: zero source_blobs.
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 0);

    // Mid-transaction: source_blob insert succeeds, then _source insert
    // fails (PK conflict — no OR IGNORE / OR REPLACE, exact match the parse
    // path). We roll back explicitly, mirroring SQLite's semantics on any
    // uncaught statement error inside a transaction.
    conn.execute_batch("BEGIN").unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
        params![blob_hash.to_vec(), blob_bytes.to_vec()],
    )
    .unwrap();
    // Verify the blob is visible mid-transaction.
    let mid: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mid, 1);

    // Now the second insert fails with the PK conflict.
    let e = conn
        .execute(
            "INSERT INTO _source (id, language, path, content_hash) \
             VALUES ('a.go', 'go', '/tmp/a.go', ?1)",
            params![blob_hash.to_vec()],
        )
        .unwrap_err();
    // Sanity-check the error is a PK conflict, not a schema issue.
    assert!(
        format!("{e}").contains("UNIQUE") || format!("{e}").contains("PRIMARY"),
        "expected UNIQUE/PRIMARY KEY error, got: {e}",
    );

    conn.execute_batch("ROLLBACK").unwrap();

    // Post-condition: source_blobs is empty again — the mid-transaction
    // insert rolled back. If this ever fails, the atomicity contract on
    // which Phase 1's F1s in-DB round-trip depends is broken.
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        after, 0,
        "atomicity: rollback MUST reverse the source_blobs insert too",
    );
}

// ── Adversarial 2: large source blob ──────────────────────────────────────

/// Synthesize a source file > 1 MiB, parse it. Assert no panic + hash
/// matches BLAKE3 of the file bytes. Guards against a per-file-size cap
/// that would silently truncate `blob_bytes` or refuse the insert.
#[test]
fn adversarial_large_source_blob() {
    let src = TempDir::new().unwrap();
    // Build a 1.5 MiB Go source file. `package main\n` prefix keeps it
    // parseable; the body is a giant comment (guaranteed valid Go, no
    // null bytes so it isn't binary-rejected).
    let mut content = Vec::with_capacity(1_600_000);
    content.extend_from_slice(b"package main\n\n// ");
    while content.len() < 1_500_000 {
        content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz0123456789 ");
    }
    content.push(b'\n');
    assert!(content.len() > 1_048_576, "must exceed 1 MiB");
    fs::write(src.path().join("big.go"), &content).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    // Only one file — must have exactly one blob whose hash matches.
    let (stored_hash, stored_bytes): (Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT b.blob_hash, b.blob_bytes FROM _source s \
             JOIN source_blobs b ON b.blob_hash = s.content_hash \
             WHERE s.id = 'big.go'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        stored_bytes.len(),
        content.len(),
        "large source: byte length preserved (no silent truncation)",
    );
    assert_eq!(
        stored_bytes, content,
        "large source: byte content preserved"
    );
    let expected = *blake3::hash(&content).as_bytes();
    assert_eq!(
        stored_hash.as_slice(),
        &expected[..],
        "large source: hash matches BLAKE3(bytes) — no truncation or split",
    );
}

// ── Adversarial 3: malformed / non-UTF-8 bytes ────────────────────────────

/// A source file with invalid UTF-8 (Latin-1 / arbitrary high bytes, no
/// null byte so it clears the binary-file guard). Assert the bytes are
/// stored verbatim and the hash matches the raw bytes — no UTF-8
/// normalization / lossy conversion happens on the write path.
#[test]
fn adversarial_malformed_bytes() {
    let src = TempDir::new().unwrap();
    // Non-UTF-8 header masquerading as a Go comment (tree-sitter tolerates
    // non-UTF-8 in tokens). No null byte → not rejected by the binary check.
    let mut content: Vec<u8> = b"package main\n\n// ".to_vec();
    content.extend_from_slice(&[0xff, 0xfe, 0xfd, 0xfc, 0xc0, 0xc1, 0xf5, 0xf6, 0xf7, 0xf8]);
    content.extend_from_slice(b"\nfunc x() {}\n");
    // Sanity: this really is invalid UTF-8.
    assert!(std::str::from_utf8(&content).is_err());
    fs::write(src.path().join("bad.go"), &content).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let (stored_hash, stored_bytes): (Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT b.blob_hash, b.blob_bytes FROM _source s \
             JOIN source_blobs b ON b.blob_hash = s.content_hash \
             WHERE s.id = 'bad.go'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        stored_bytes, content,
        "malformed bytes: stored verbatim (no UTF-8 normalization)",
    );
    let expected = *blake3::hash(&content).as_bytes();
    assert_eq!(
        stored_hash.as_slice(),
        &expected[..],
        "malformed bytes: hash matches BLAKE3(raw bytes)",
    );
}
