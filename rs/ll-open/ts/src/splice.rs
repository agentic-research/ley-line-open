//! AST splice: the byte arithmetic of an edit to one node, and the one
//! reader of a file's bytes.
//!
//! A write to a node is `source[..start] + new_text + source[end..]`. What
//! happens to those bytes next — the blob store, the manifest, the
//! re-projection, `treeRoot` — is ADR-0040 D3's one write path
//! (`leyline_cli_lib::source_write::write`), which every writer calls. This
//! module never writes the arena.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use tree_sitter::{Language, Parser};

/// Splice new text into a node's byte range, returning the modified source bytes.
///
/// Reads the node's `start_byte`/`end_byte` from `_ast` and the original source
/// through [`source_bytes`], then performs: `source[..start] + new_text + source[end..]`.
///
/// projection-v5: keyed on the integer `nid`; the node's file is `nid >> 24`,
/// joined to `_source.file_id`. Callers holding a display path resolve it
/// first via [`crate::schema::resolve_path`].
pub fn splice(conn: &Connection, nid: i64, new_text: &str) -> Result<Vec<u8>> {
    // Look up byte range from _ast
    let (start_byte, end_byte): (usize, usize) = conn
        .query_row(
            "SELECT start_byte, end_byte FROM _ast WHERE nid = ?1",
            [nid],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )
        .with_context(|| format!("node {nid} not found in _ast table"))?;
    let file_id = leyline_schema::nid_file_id(nid)
        .with_context(|| format!("node {nid} is a directory nid — nothing to splice"))?;

    let source = source_bytes(conn, file_id)?;

    if start_byte > source.len() || end_byte > source.len() || start_byte > end_byte {
        bail!(
            "invalid byte range [{}, {}) for source of {} bytes",
            start_byte,
            end_byte,
            source.len()
        );
    }

    // Splice: before + new_text + after
    let mut result = Vec::with_capacity(start_byte + new_text.len() + (source.len() - end_byte));
    result.extend_from_slice(&source[..start_byte]);
    result.extend_from_slice(new_text.as_bytes());
    result.extend_from_slice(&source[end_byte..]);

    Ok(result)
}

/// The bytes of the file `file_id` names, verified against its
/// `_source.content_hash`.
///
/// Two homes, read in this order: inline `_source.content` (the single-file
/// projector's shape), then `source_blobs` under `content_hash` (ADR-0028,
/// every cold-parsed and every written file). When the row carries a hash,
/// the bytes returned are the bytes it names or an error: a hash with no
/// blob row fails closed (bead `ley-line-open-0d3b72`), and bytes that do
/// not hash to it are refused. The file on disk is never read — it is the
/// working tree, not the arena's record of the file. A chunk-activated blob
/// (bytes moved into chunks by `leyline-fs`) is reported as such rather than
/// read wrongly.
pub fn source_bytes(conn: &Connection, file_id: i64) -> Result<Vec<u8>> {
    /// The `_source` columns a file's bytes can live behind.
    struct SourceRow {
        id: String,
        content: Option<Vec<u8>>,
        content_hash: Option<Vec<u8>>,
    }
    let SourceRow {
        id,
        content,
        content_hash,
    } = conn
        .query_row(
            "SELECT id, content, content_hash FROM _source WHERE file_id = ?1",
            [file_id],
            |r| {
                Ok(SourceRow {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    content_hash: r.get(2)?,
                })
            },
        )
        .with_context(|| format!("source for file_id {file_id} not found in _source table"))?;
    let bytes = match (content, &content_hash) {
        (Some(c), _) => c,
        (None, Some(hash)) => {
            let blob: Option<Option<Vec<u8>>> =
                if leyline_schema::table_exists(conn, "source_blobs")? {
                    conn.query_row(
                        "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1",
                        [hash],
                        |r| r.get(0),
                    )
                    .optional()?
                } else {
                    None
                };
            match blob {
                Some(Some(bytes)) => bytes,
                Some(None) => bail!(
                    "source '{id}' is chunk-activated in source_blobs; read it through leyline-fs"
                ),
                None => bail!(
                    "source '{id}' names content hash {} but source_blobs holds no such blob",
                    hex_of(hash)
                ),
            }
        }
        (None, None) => bail!("source '{id}' has neither inline content nor a content hash"),
    };
    if let Some(hash) = content_hash {
        let actual = {
            use leyline_core::substrate::ContentAddressed;
            *bytes.hash().as_bytes()
        };
        if actual[..] != hash[..] {
            bail!(
                "source '{id}': stored bytes hash to {} but _source.content_hash is {}",
                hex_of(&actual),
                hex_of(&hash)
            );
        }
    }
    Ok(bytes)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Refuse `source` if it does not parse cleanly in `language`, naming the
/// first error or missing node. A write through a mount lands one edit at a
/// time; an edit that leaves the file unparseable is rejected rather than
/// projected.
pub fn check_syntax(source: &[u8], language: Language) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("failed to set tree-sitter language")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter parse returned None")?;
    if !tree.root_node().has_error() {
        return Ok(());
    }
    // Find the first ERROR node and report its byte range
    let mut cursor = tree.walk();
    let mut error_info = String::new();
    'walk: loop {
        if cursor.node().is_error() || cursor.node().is_missing() {
            let node = cursor.node();
            error_info = format!(
                " (error at byte {}..{}, line {})",
                node.start_byte(),
                node.end_byte(),
                node.start_position().row + 1,
            );
            break 'walk;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    bail!("modified source has syntax errors{error_info} — splice rejected")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "html")]
    use crate::project::project_ast_with_source;
    use rusqlite::Connection;

    #[cfg(feature = "html")]
    fn setup_html(src: &[u8]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        let lang: Language = tree_sitter_html::LANGUAGE.into();
        project_ast_with_source(src, lang, &conn, "test.html", "html").unwrap();
        conn
    }

    /// Resolve a display path to its nid — the v5 addressing boundary.
    #[cfg(feature = "html")]
    fn nid_of(conn: &Connection, path: &str) -> i64 {
        crate::schema::resolve_path(conn, path)
            .unwrap()
            .unwrap_or_else(|| panic!("path must resolve: {path:?}"))
    }

    #[cfg(feature = "html")]
    fn get_ast_range(conn: &Connection, path: &str) -> (i64, i64) {
        let nid = nid_of(conn, path);
        conn.query_row(
            "SELECT start_byte, end_byte FROM _ast WHERE nid = ?1",
            [nid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_replaces_byte_range() {
        let conn = setup_html(b"<p>Hello</p>");
        // The file node is "test.html"; children render as
        // "test.html/element", "test.html/element/text", etc.
        let (start, end) = get_ast_range(&conn, "test.html/element/text");
        assert_eq!(start, 3);
        assert_eq!(end, 8);

        let result = splice(&conn, nid_of(&conn, "test.html/element/text"), "World").unwrap();
        assert_eq!(result, b"<p>World</p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_at_start() {
        // The file's own node (ordinal 0, the AST root) spans the entire source
        let conn = setup_html(b"<p>Hi</p>");
        let result = splice(&conn, nid_of(&conn, "test.html"), "<div>New</div>").unwrap();
        assert_eq!(result, b"<div>New</div>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_expand() {
        let conn = setup_html(b"<p>Hi</p>");
        let result = splice(
            &conn,
            nid_of(&conn, "test.html/element/text"),
            "Hello World",
        )
        .unwrap();
        assert_eq!(result, b"<p>Hello World</p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_delete() {
        let conn = setup_html(b"<p>Hello</p>");
        let result = splice(&conn, nid_of(&conn, "test.html/element/text"), "").unwrap();
        assert_eq!(result, b"<p></p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_nonexistent_node_fails() {
        let conn = setup_html(b"<p>Hello</p>");
        // A nid far outside any interned file's populated range.
        let result = splice(&conn, 1 << 40, "text");
        assert!(result.is_err());
    }

    #[cfg(feature = "json")]
    #[test]
    fn check_syntax_accepts_clean_source_and_names_the_first_error() {
        let json = crate::languages::TsLanguage::Json.ts_language();
        check_syntax(br#"{"a": 1}"#, json.clone()).unwrap();
        let err = check_syntax(b"{\"a\": 1,\n \"b\": }", json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("syntax errors"), "{msg}");
        assert!(msg.contains("line 2"), "the error names its line: {msg}");
    }

    /// A `_source` row in the cold parse's shape: no inline content, a
    /// content hash keying `source_blobs`.
    fn blob_arena(hash: &[u8], blob: Option<&[u8]>) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::create_ast_tables(&conn).unwrap();
        crate::schema::create_source_blobs_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO _source (id, language, path, content_hash, file_id) \
             VALUES ('a.go', 'go', '/nonexistent/a.go', ?1, 7)",
            [hash],
        )
        .unwrap();
        if let Some(b) = blob {
            conn.execute(
                "INSERT INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
                rusqlite::params![hash, b],
            )
            .unwrap();
        }
        conn
    }

    fn hash_of(bytes: &[u8]) -> Vec<u8> {
        use leyline_core::substrate::ContentAddressed;
        bytes.hash().as_bytes().to_vec()
    }

    #[test]
    fn source_bytes_reads_the_blob_its_hash_names() {
        let h = hash_of(b"package a\n");
        let conn = blob_arena(&h, Some(b"package a\n"));
        assert_eq!(source_bytes(&conn, 7).unwrap(), b"package a\n");
    }

    /// Bead `0d3b72`: a hash with no blob row is an error, never a fall
    /// through to the file on disk.
    #[test]
    fn source_bytes_fails_closed_on_a_hash_with_no_blob() {
        let h = hash_of(b"package a\n");
        let conn = blob_arena(&h, None);
        let msg = format!("{:#}", source_bytes(&conn, 7).unwrap_err());
        assert!(msg.contains("holds no such blob"), "{msg}");
        assert!(msg.contains(&hex_of(&h)), "the error names the hash: {msg}");
    }

    #[test]
    fn source_bytes_refuses_a_blob_that_does_not_hash_to_its_key() {
        let key = hash_of(b"package a\n");
        let conn = blob_arena(&key, Some(b"package b\n"));
        let msg = format!("{:#}", source_bytes(&conn, 7).unwrap_err());
        assert!(msg.contains("hash to"), "{msg}");
        assert!(msg.contains(&hex_of(&key)), "names the key: {msg}");
        assert!(
            msg.contains(&hex_of(&hash_of(b"package b\n"))),
            "names the bytes' hash: {msg}"
        );
    }

    #[test]
    fn hex_of_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex_of(&[0x00, 0x0a, 0xff]), "000aff");
    }

    #[test]
    fn source_bytes_verifies_inline_content_against_a_stored_hash() {
        let conn = blob_arena(&hash_of(b"package a\n"), None);
        conn.execute("UPDATE _source SET content = X'00' WHERE file_id = 7", [])
            .unwrap();
        let err = source_bytes(&conn, 7).unwrap_err();
        assert!(format!("{err:#}").contains("hash to"), "{err:#}");
        conn.execute(
            "UPDATE _source SET content = ?1 WHERE file_id = 7",
            [b"package a\n".as_slice()],
        )
        .unwrap();
        assert_eq!(source_bytes(&conn, 7).unwrap(), b"package a\n");
    }

    #[test]
    fn source_bytes_reads_inline_content_with_no_hash_and_refuses_a_row_with_neither() {
        let conn = blob_arena(&hash_of(b"x"), None);
        conn.execute(
            "UPDATE _source SET content = X'41', content_hash = NULL WHERE file_id = 7",
            [],
        )
        .unwrap();
        assert_eq!(source_bytes(&conn, 7).unwrap(), b"A");
        conn.execute("UPDATE _source SET content = NULL WHERE file_id = 7", [])
            .unwrap();
        let err = source_bytes(&conn, 7).unwrap_err();
        assert!(format!("{err:#}").contains("neither"), "{err:#}");
    }
}
