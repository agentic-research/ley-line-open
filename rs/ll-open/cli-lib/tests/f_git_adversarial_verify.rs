//! ADR-0028 F-git — INDEPENDENT adversarial verification.
//!
//! Bead: verification for ADR-0029's foundation. ADR-0029 plans to use
//! `source_blobs` as a git-object-compatible CAS, so F-git must hold under
//! adversarial probing before that design lands.
//!
//! ADR-0028 §4 F-git claim:
//!   "BLAKE3-source-blobs are conceptually identical to git blobs under
//!    BLAKE3. Ingesting a `.git/objects` blob and computing its BLAKE3
//!    matches what LLO would produce for the same source bytes."
//!
//! This test intentionally does NOT depend on any LLO hashing helper — it
//! computes BLAKE3 via the `blake3` crate directly against the raw bytes
//! returned by `git cat-file blob <sha>`, then compares to the value LLO
//! wrote to `source_blobs.blob_hash`. If the two hash function surfaces
//! ever diverge (a stray prefix, encoding normalization, trailing-newline
//! munging, wire-format leak), a case here fails.
//!
//! CRITICAL invariant this test pins:
//!   `git cat-file blob <sha>` returns the RAW blob content (git strips
//!   the `blob <size>\0` header before emitting). LLO must hash the RAW
//!   content. If LLO ever hashed `blob <size>\0<content>` (the git-wire-
//!   format bytes), F-git is broken. The `wire_format_not_hashed` case
//!   pins this explicitly.
//!
//! Cases probed:
//!   1. `hello world\n` — canonical git example
//!   2. Empty file — 0 bytes; exercises the binary-guard's `content[..0]` edge
//!   3. Trailing newline off — `foo` (no NL)
//!   4. Trailing newline on — `foo\n`
//!   5. UTF-8 BOM — leading `EF BB BF` bytes
//!   6. 1-byte file — smallest non-empty
//!   7. Binary bytes without a NUL in first 8KB — bypasses LLO's binary
//!      guard so we can compare hashes on non-UTF-8 payloads
//!   8. Git wire format NOT hashed — asserts LLO hashes raw content, not
//!      `blob <len>\0<content>`

use std::fs;
use std::path::Path;
use std::process::Command;

use blake3;
use leyline_cli_lib::cmd_parse::parse_into_conn;
use rusqlite::Connection;
use tempfile::TempDir;

/// Initialize a git repo in `dir` with `core.autocrlf=false` (so `git add`
/// can't silently rewrite text bytes on a CRLF-configured host) and a
/// throw-away identity for commits. gpg signing off — if the developer's
/// global config requires signing, otherwise `git commit` would fail.
fn git_init(dir: &Path) {
    let ok = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    };
    ok(&["init", "-q", "-b", "main"]);
    ok(&["config", "core.autocrlf", "false"]);
    ok(&["config", "user.name", "F-git verifier"]);
    ok(&["config", "user.email", "verifier@test.local"]);
    ok(&["config", "commit.gpgsign", "false"]);
}

/// `git hash-object -w <rel_path>` — stores the blob and returns its SHA-1.
/// This is git's canonical "give me the blob SHA for these bytes"
/// primitive; it's what `git add` uses internally.
fn git_hash_object(dir: &Path, rel_path: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["hash-object", "-w", "--"])
        .arg(rel_path)
        .output()
        .expect("run git hash-object");
    assert!(
        out.status.success(),
        "git hash-object failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `git cat-file blob <sha>` — returns the RAW blob content. git strips
/// the `blob <size>\0` wire-format header before emitting to stdout.
fn git_cat_file_blob(dir: &Path, sha: &str) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "blob", sha])
        .output()
        .expect("run git cat-file");
    assert!(
        out.status.success(),
        "git cat-file blob {sha} failed: {:?}",
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

/// Read `source_blobs.blob_hash` for a given path via the FK from `_source`.
/// Returns `None` when LLO didn't produce a row for that file (e.g. the
/// parse worker rejected it — the binary-file guard bails on NUL in the
/// first 8KB).
fn lookup_source_blob_hash(conn: &Connection, rel_path: &str) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT sb.blob_hash FROM _source s \
         JOIN source_blobs sb ON s.content_hash = sb.blob_hash \
         WHERE s.id = ?1",
        [rel_path],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .ok()
}

/// Read `source_blobs.blob_bytes` for the same joined row.
fn lookup_source_blob_bytes(conn: &Connection, rel_path: &str) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT sb.blob_bytes FROM _source s \
         JOIN source_blobs sb ON s.content_hash = sb.blob_hash \
         WHERE s.id = ?1",
        [rel_path],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .ok()
}

/// Verify a single-file case: raw bytes → git blob → BLAKE3(git blob
/// bytes) → LLO parse → assert LLO's `source_blobs.blob_hash` matches.
///
/// The `expect_llo_row` flag controls whether LLO producing NO row is a
/// tolerated outcome (for the binary-with-NUL case, LLO's parse worker
/// rejects the file — that's an ingest policy, not a hash-equality bug).
fn verify_case(case: &str, bytes: &[u8], extension: &str, expect_llo_row: bool) {
    let dir = TempDir::new().expect("tempdir");
    git_init(dir.path());

    let filename = format!("case_{case}.{extension}");
    fs::write(dir.path().join(&filename), bytes).expect("write test file");

    // Ingest via `git hash-object -w`, then extract via `cat-file blob`.
    // Round-trips through git's actual storage — the same code path that
    // any `.git/objects` file would exercise.
    let sha = git_hash_object(dir.path(), &filename);
    let git_blob = git_cat_file_blob(dir.path(), &sha);

    // Sanity check: git returns EXACTLY the bytes we wrote. If not, git
    // is applying some conversion (autocrlf leak, encoding filter) and
    // the test setup is wrong — abort loudly so a false pass can't hide
    // an autocrlf misconfiguration.
    assert_eq!(
        git_blob.as_slice(),
        bytes,
        "[{case}] git cat-file blob != on-disk bytes ({} vs {} bytes) — \
         autocrlf leak or filter driver?",
        git_blob.len(),
        bytes.len(),
    );

    // INDEPENDENT BLAKE3 — via the crate directly, NEVER through any LLO
    // hash helper. If LLO's hash surface ever drifts from raw BLAKE3
    // (adds a prefix, applies canonicalization), this expected value
    // won't match LLO's actual value.
    let expected: [u8; 32] = *blake3::hash(&git_blob).as_bytes();

    // Now feed the same content through LLO's parse pipeline.
    let conn = Connection::open_in_memory().expect("in-memory conn");
    let r = parse_into_conn(&conn, dir.path(), None, None).expect("parse_into_conn");

    let actual = lookup_source_blob_hash(&conn, &filename);

    match (actual, expect_llo_row) {
        (Some(actual_hash), _) => {
            assert_eq!(
                actual_hash.as_slice(),
                expected.as_slice(),
                "[{case}] LLO source_blobs.blob_hash ({}) != BLAKE3(git blob bytes) ({}); \
                 input bytes ({} bytes): {:?}",
                hex::encode(&actual_hash),
                hex::encode(expected),
                bytes.len(),
                bytes,
            );

            // blob_bytes byte-identity — the storage of the raw content
            // must equal git's stored blob (the whole point of unified
            // CAS composition).
            let actual_bytes =
                lookup_source_blob_bytes(&conn, &filename).expect("blob_bytes present");
            assert_eq!(
                actual_bytes.as_slice(),
                bytes,
                "[{case}] LLO source_blobs.blob_bytes != git blob bytes",
            );

            eprintln!(
                "[{case}] CONFIRMED — blob_hash = {} ({} bytes)",
                hex::encode(expected),
                bytes.len(),
            );
        }
        (None, true) => {
            panic!(
                "[{case}] LLO produced NO source_blobs row (parsed={} errors={}); \
                 F-git can't be validated when LLO refuses to ingest",
                r.parsed, r.errors,
            );
        }
        (None, false) => {
            eprintln!(
                "[{case}] LLO refused to ingest (parsed={} errors={}); F-git \
                 tolerated skip (LLO ingest policy is orthogonal to hash equality)",
                r.parsed, r.errors,
            );
        }
    }
}

// ─── Phase 2 — the canonical example ──────────────────────────────────────

#[test]
fn f_git_case_1_hello_world() {
    // Canonical git worked example. "hello world\n" has git blob SHA-1
    // 3b18e512dba79e4c8300dd08aeb37f8e728b8dad (widely-published constant).
    // BLAKE3 of the same 12 bytes must match LLO's blob_hash.
    verify_case("hello_world", b"hello world\n", "py", true);
}

// ─── Phase 3 — adversarial edge cases ─────────────────────────────────────

#[test]
fn f_git_case_2_empty_file() {
    // 0-byte file. Git blob SHA-1 = e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
    // (git's canonical "empty" blob). BLAKE3(empty) =
    // af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262.
    // LLO's binary-guard evaluates `content[..0].contains(&0)` on
    // empty input — must not spuriously reject.
    verify_case("empty_file", b"", "py", true);
}

#[test]
fn f_git_case_3_trailing_newline_absent() {
    // "foo" — no trailing NL. Git renders "\ No newline at end of file"
    // in `git diff` for such files but stores raw bytes. Distinct blob
    // from case 4.
    verify_case("no_trailing_nl", b"foo", "py", true);
}

#[test]
fn f_git_case_4_trailing_newline_present() {
    // "foo\n" — with trailing NL. Must hash differently from case 3
    // (byte-different content ⇒ different blob).
    verify_case("with_trailing_nl", b"foo\n", "py", true);
}

#[test]
fn f_git_case_5_utf8_bom() {
    // UTF-8 BOM at start (EF BB BF). Some tools normalize the BOM;
    // git and LLO must both store the bytes verbatim.
    verify_case("utf8_bom", b"\xEF\xBB\xBF#hi\n", "py", true);
}

#[test]
fn f_git_case_6_one_byte() {
    // Smallest non-empty file. Pin: single-byte content round-trips
    // without any special short-file path masking a hash mismatch.
    verify_case("one_byte", b"x", "py", true);
}

#[test]
fn f_git_case_7_binary_bytes_no_nul() {
    // Adversarial: non-UTF-8 bytes but NO NUL in the first 8KB, so LLO's
    // binary-file guard (`content[..min(8192,len)].contains(&0)`) doesn't
    // fire. If LLO ever normalized encoding (e.g. lossy-UTF-8-decoded
    // before hashing), the hash would drift here.
    let bytes: &[u8] = &[0x01, 0x7F, 0x80, 0xFF, 0xC0, 0xC1, b'\n', b'x', b'\n'];
    verify_case("bin_no_nul", bytes, "py", true);
}

// ─── Phase 3 — the critical wire-format check ─────────────────────────────

#[test]
fn f_git_case_8_wire_format_not_hashed() {
    // CRITICAL: `git cat-file blob <sha>` returns the RAW content
    // (git strips the `blob <size>\0` header before emitting). If LLO
    // ever hashed the WIRE FORMAT (`blob <size>\0<content>`) instead
    // of the raw content, F-git is broken — same file bytes would
    // produce different hashes in LLO vs the canonical BLAKE3-of-git-
    // blob-bytes we plan to use for unified-CAS composition.
    //
    // This case asserts BOTH:
    //   - LLO's blob_hash == BLAKE3(raw content)
    //   - LLO's blob_hash != BLAKE3("blob <size>\0" ‖ raw content)

    let dir = TempDir::new().expect("tempdir");
    git_init(dir.path());
    let content = b"hello world\n";
    let filename = "case_wire.py";
    fs::write(dir.path().join(filename), content).expect("write");

    let sha = git_hash_object(dir.path(), filename);
    let raw = git_cat_file_blob(dir.path(), &sha);
    let raw_hash: [u8; 32] = *blake3::hash(&raw).as_bytes();

    // Reconstruct git's wire format: `blob <len>\0<content>`.
    let wire = {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(format!("blob {}\0", raw.len()).as_bytes());
        v.extend_from_slice(&raw);
        v
    };
    let wire_hash: [u8; 32] = *blake3::hash(&wire).as_bytes();

    // BLAKE3 collision would be a bigger problem than F-git — but pin it.
    assert_ne!(
        raw_hash, wire_hash,
        "raw-content BLAKE3 == wire-format BLAKE3 — either a BLAKE3 collision \
         (astronomically unlikely) or a test-setup bug"
    );

    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, dir.path(), None, None).unwrap();

    let llo_hash: Vec<u8> =
        lookup_source_blob_hash(&conn, filename).expect("source_blobs row must exist");

    assert_eq!(
        llo_hash.as_slice(),
        raw_hash.as_slice(),
        "LLO must hash RAW content ({}), not git wire format ({})",
        hex::encode(raw_hash),
        hex::encode(wire_hash),
    );
    assert_ne!(
        llo_hash.as_slice(),
        wire_hash.as_slice(),
        "LLO must NOT hash the git-wire-format-prefixed bytes",
    );
    eprintln!(
        "[wire_format_check] CONFIRMED — LLO hashes raw content ({}), \
         wire-format ({}) is distinct and unused",
        hex::encode(raw_hash),
        hex::encode(wire_hash),
    );
}
