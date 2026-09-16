//! `source_bytes` serves a file's bytes on an arena the parser built
//! (bead ley-line-open-af4539).
//!
//! `batch_splice` in `leyline-fs` read `SELECT content FROM _source`, but
//! `parse_into_conn` writes `_source(id, language, path, content_hash,
//! file_id)` and the bytes themselves into `source_blobs`. `content` is NULL
//! on every daemon-parsed arena, so every mount write failed with
//! "source not found". There is now one reader, `leyline_ts::splice::
//! source_bytes`, used by `splice` and `batch_splice` alike: inline content
//! first, then the blob the `content_hash` points at, then the path on disk.
//! This parses a real Go file, deletes it from disk so the path can't be the
//! answer, and reads the bytes back from the projection.

use std::fs;

use tempfile::TempDir;

#[test]
fn source_bytes_come_from_the_projection_not_the_disk() {
    let dir = TempDir::new().unwrap();
    let source = b"package main\n\nfunc alpha() int {\n\treturn 1\n}\n";
    fs::write(dir.path().join("a.go"), source).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, dir.path(), Some("go"), None)
        .expect("cold parse");
    let file_id = leyline_schema::lookup_file_id(&conn, "a.go")
        .unwrap()
        .expect("a.go interned");

    // The parser never writes `_source.content`; a reader keyed on it has
    // nothing to read.
    let content: Option<Vec<u8>> = conn
        .query_row(
            "SELECT content FROM _source WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        content.is_none(),
        "fixture: the parser stores no inline content"
    );

    // With the file gone, only the projection can answer.
    fs::remove_file(dir.path().join("a.go")).unwrap();
    let bytes = leyline_ts::splice::source_bytes(&conn, file_id).expect("source bytes");
    assert_eq!(bytes, source);
}
