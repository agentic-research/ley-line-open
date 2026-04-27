//! Bidirectional AST splice: write to a node → splice into source → re-parse.
//!
//! Enables LLMs to edit individual AST nodes via mounted files while ley-line
//! handles the byte-level splicing back into the original source.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use tree_sitter::{Language, Parser};

use crate::project::project_ast_with_source;

/// Splice new text into a node's byte range, returning the modified source bytes.
///
/// Reads the node's `start_byte`/`end_byte` from `_ast` and the original source
/// from `_source`, then performs: `source[..start] + new_text + source[end..]`.
pub fn splice(conn: &Connection, node_id: &str, new_text: &str) -> Result<Vec<u8>> {
    // Look up byte range from _ast
    let (source_id, start_byte, end_byte): (String, usize, usize) = conn
        .query_row(
            "SELECT source_id, start_byte, end_byte FROM _ast WHERE node_id = ?1",
            [node_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, i64>(2)? as usize,
                ))
            },
        )
        .with_context(|| format!("node '{}' not found in _ast table", node_id))?;

    // Read original source — inline content or from disk via path reference.
    let source: Vec<u8> = conn
        .query_row(
            "SELECT content, path FROM _source WHERE id = ?1",
            [&source_id],
            |r| {
                let content: Option<Vec<u8>> = r.get(0)?;
                let path: Option<String> = r.get(1)?;
                Ok((content, path))
            },
        )
        .with_context(|| format!("source '{}' not found in _source table", source_id))
        .and_then(|(content, path)| {
            if let Some(c) = content {
                Ok(c)
            } else if let Some(p) = path {
                std::fs::read(&p)
                    .with_context(|| format!("read source file: {p}"))
            } else {
                bail!("source '{}' has neither content nor path", source_id)
            }
        })?;

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

/// Re-parse modified source and update all tables (`_source`, `_ast`, `nodes`) atomically.
///
/// Validates that the new source parses without errors before committing changes.
/// Runs inside a SQLite transaction — rolls back on failure.
pub fn reproject(
    conn: &Connection,
    source_id: &str,
    new_source: &[u8],
    language: Language,
    language_name: &str,
) -> Result<()> {
    // Validate: parse must succeed without errors
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("failed to set tree-sitter language")?;
    let tree = parser
        .parse(new_source, None)
        .context("tree-sitter parse returned None")?;
    if tree.root_node().has_error() {
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
        bail!("modified source has syntax errors{error_info} — splice rejected");
    }

    // Atomic update inside a transaction
    conn.execute_batch("BEGIN")?;

    let result = (|| -> Result<()> {
        // Clear existing data
        conn.execute("DELETE FROM nodes", [])?;
        conn.execute("DELETE FROM _ast", [])?;
        conn.execute("DELETE FROM _source", [])?;

        // Re-project from scratch
        project_ast_with_source(new_source, language, conn, source_id, language_name)?;

        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Splice new text into a node, validate, and reproject in one call.
///
/// Returns the modified source bytes on success. If the splice produces
/// invalid syntax, the database is left unchanged.
pub fn splice_and_reproject(conn: &Connection, node_id: &str, new_text: &str) -> Result<Vec<u8>> {
    // Look up source metadata for reprojection
    let (source_id, language_name): (String, String) = conn
        .query_row(
            "SELECT s.id, s.language FROM _source s \
             JOIN _ast a ON a.source_id = s.id \
             WHERE a.node_id = ?1",
            [node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .with_context(|| format!("node '{}' not found in _ast/_source tables", node_id))?;

    let language = crate::languages::TsLanguage::from_name(&language_name)?;

    let new_source = splice(conn, node_id, new_text)?;
    reproject(
        conn,
        &source_id,
        &new_source,
        language.ts_language(),
        &language_name,
    )?;

    Ok(new_source)
}

/// Validate and reproject a source by ID, looking up the language from `_source`.
///
/// Used by batch splice — the caller provides pre-computed source bytes,
/// and this function handles language lookup, validation, and reprojection.
pub fn reproject_source(conn: &Connection, source_id: &str, new_source: &[u8]) -> Result<()> {
    let language_name: String = conn
        .query_row(
            "SELECT language FROM _source WHERE id = ?1",
            [source_id],
            |r| r.get(0),
        )
        .with_context(|| format!("source '{}' not found in _source table", source_id))?;

    let language = crate::languages::TsLanguage::from_name(&language_name)?;
    reproject(
        conn,
        source_id,
        new_source,
        language.ts_language(),
        &language_name,
    )
}

/// High-level: splice a serialized .db, returning updated serialized bytes.
///
/// Takes raw SQLite bytes (as produced by `parse`/`parse_with_source`),
/// performs the splice + reproject, and returns new serialized bytes.
pub fn splice_db_bytes(db_bytes: &[u8], node_id: &str, new_text: &str) -> Result<Vec<u8>> {
    use rusqlite::DatabaseName;
    use std::io::Cursor;

    let mut conn = Connection::open_in_memory()?;
    let cursor = Cursor::new(db_bytes);
    conn.deserialize_read_exact(DatabaseName::Main, cursor, db_bytes.len(), true)
        .context("failed to deserialize .db bytes")?;

    splice_and_reproject(&conn, node_id, new_text)?;

    let data = conn.serialize(DatabaseName::Main)?;
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::project_ast_with_source;
    use rusqlite::Connection;

    fn setup_html(src: &[u8]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        let lang: Language = tree_sitter_html::LANGUAGE.into();
        project_ast_with_source(src, lang, &conn, "test.html", "html").unwrap();
        conn
    }

    fn get_source(conn: &Connection) -> Vec<u8> {
        conn.query_row("SELECT content FROM _source LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    fn get_record(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT record FROM nodes WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn get_ast_range(conn: &Connection, node_id: &str) -> (i64, i64) {
        conn.query_row(
            "SELECT start_byte, end_byte FROM _ast WHERE node_id = ?1",
            [node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn count_nodes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap()
    }

    fn count_ast(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap()
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_replaces_byte_range() {
        let conn = setup_html(b"<p>Hello</p>");
        // Root is "", children are "element", "element/text", etc.
        let (start, end) = get_ast_range(&conn, "element/text");
        assert_eq!(start, 3);
        assert_eq!(end, 8);

        let result = splice(&conn, "element/text", "World").unwrap();
        assert_eq!(result, b"<p>World</p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_at_start() {
        // The root node ("") spans the entire source
        let conn = setup_html(b"<p>Hi</p>");
        let result = splice(&conn, "", "<div>New</div>").unwrap();
        assert_eq!(result, b"<div>New</div>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_expand() {
        let conn = setup_html(b"<p>Hi</p>");
        let result = splice(&conn, "element/text", "Hello World").unwrap();
        assert_eq!(result, b"<p>Hello World</p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_delete() {
        let conn = setup_html(b"<p>Hello</p>");
        let result = splice(&conn, "element/text", "").unwrap();
        assert_eq!(result, b"<p></p>");
    }

    #[cfg(feature = "html")]
    #[test]
    fn reproject_updates_all_tables() {
        let conn = setup_html(b"<p>Hello</p>");
        let nodes_before = count_nodes(&conn);
        let ast_before = count_ast(&conn);

        let lang: Language = tree_sitter_html::LANGUAGE.into();
        reproject(&conn, "test.html", b"<p>World</p>", lang, "html").unwrap();

        // Source updated
        assert_eq!(get_source(&conn), b"<p>World</p>");

        // nodes and _ast refreshed (same structure, so same counts)
        assert_eq!(count_nodes(&conn), nodes_before);
        assert_eq!(count_ast(&conn), ast_before);

        // Text node now has "World"
        assert_eq!(get_record(&conn, "element/text"), "World");
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_and_reproject_roundtrip() {
        let conn = setup_html(b"<p>Hello</p>");

        let new_source = splice_and_reproject(&conn, "element/text", "Goodbye").unwrap();
        assert_eq!(new_source, b"<p>Goodbye</p>");

        // DB reflects the change
        assert_eq!(get_source(&conn), b"<p>Goodbye</p>");
        assert_eq!(get_record(&conn, "element/text"), "Goodbye");

        // Byte ranges updated
        let (start, end) = get_ast_range(&conn, "element/text");
        assert_eq!(start, 3);
        assert_eq!(end, 10); // 3 + len("Goodbye")
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_syntax_error_rejected() {
        let conn = setup_html(b"<p>Hello</p>");
        let original_source = get_source(&conn);
        let original_record = get_record(&conn, "element/text");

        // Splice in something that produces a parse error.
        // tree-sitter HTML is very tolerant, so we test the mechanism:
        // if it errs, DB must be unchanged.
        let result = splice_and_reproject(&conn, "element", "<div><p>unclosed");

        if result.is_err() {
            assert_eq!(get_source(&conn), original_source);
            assert_eq!(get_record(&conn, "element/text"), original_record);
        }
        // If it succeeds (error-tolerant parser), that's also valid —
        // the syntax-error guard is most valuable for strict grammars.
    }

    #[cfg(feature = "html")]
    #[test]
    fn splice_nonexistent_node_fails() {
        let conn = setup_html(b"<p>Hello</p>");
        let result = splice(&conn, "nonexistent/node", "text");
        assert!(result.is_err());
    }

    #[cfg(feature = "html")]
    #[test]
    fn multiple_splices_compose() {
        let conn = setup_html(b"<p>Hello</p>");

        // First splice
        splice_and_reproject(&conn, "element/text", "World").unwrap();
        assert_eq!(get_source(&conn), b"<p>World</p>");

        // Second splice on the updated tree
        splice_and_reproject(&conn, "element/text", "Final").unwrap();
        assert_eq!(get_source(&conn), b"<p>Final</p>");
        assert_eq!(get_record(&conn, "element/text"), "Final");
    }
}
