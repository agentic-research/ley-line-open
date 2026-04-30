//! Core algorithm: walk a tree-sitter AST and project it into the `nodes` table.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tree_sitter::{Language, Parser, TreeCursor};

use crate::schema::{create_ast_schema, insert_ast, insert_node, insert_source};

/// Walk the tree-sitter AST and insert all named nodes into the database,
/// storing source text and byte-range mappings for bidirectional splicing.
///
/// - Named nodes with named children become directories (kind=1, empty record).
/// - Named leaf nodes become files (kind=0, record = plain text from source).
/// - Anonymous nodes (punctuation, operators) are skipped.
/// - Siblings with the same `kind()` get disambiguated: `element_0`, `element_1`, etc.
/// - Original source stored in `_source` table; byte ranges in `_ast` table.
pub fn project_ast_with_source(
    content: &[u8],
    language: Language,
    conn: &Connection,
    source_id: &str,
    language_name: &str,
) -> Result<()> {
    create_ast_schema(conn)?;

    // Store original source
    insert_source(conn, source_id, language_name, content)?;

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("failed to set tree-sitter language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse returned None")?;

    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;

    // Insert root directory node
    let root = tree.root_node();
    insert_node(conn, "", "", "", 1, 0, mtime, "")?;
    insert_ast(
        conn,
        "",
        source_id,
        root.kind(),
        root.start_byte(),
        root.end_byte(),
        root.start_position().row,
        root.start_position().column,
        root.end_position().row,
        root.end_position().column,
    )?;

    let mut cursor = root.walk();
    walk_children(content, &mut cursor, "", mtime, conn, source_id)?;

    Ok(())
}

/// Backward-compatible wrapper: calls `project_ast_with_source` with a default source ID.
pub fn project_ast(content: &[u8], language: Language, conn: &Connection) -> Result<()> {
    project_ast_with_source(content, language, conn, "_default", "unknown")
}

/// Recursively walk named children of the current cursor node.
fn walk_children(
    content: &[u8],
    cursor: &mut TreeCursor,
    parent_id: &str,
    mtime: i64,
    conn: &Connection,
    source_id: &str,
) -> Result<()> {
    let node = cursor.node();

    // Collect named children and count kinds for disambiguation
    let mut children = Vec::new();
    let mut kind_counts: HashMap<&str, usize> = HashMap::new();

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.is_named() {
                *kind_counts.entry(child.kind()).or_insert(0) += 1;
                children.push(child);
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    // Track per-kind index for disambiguation
    let mut kind_indices: HashMap<&str, usize> = HashMap::new();

    for child in &children {
        let kind = child.kind();
        let needs_suffix = kind_counts[kind] > 1;

        let name = if needs_suffix {
            let idx = kind_indices.entry(kind).or_insert(0);
            let n = format!("{kind}_{idx}");
            *idx += 1;
            n
        } else {
            kind.to_string()
        };

        let id = if parent_id.is_empty() {
            name.clone()
        } else {
            format!("{parent_id}/{name}")
        };

        let has_named_children = has_named_child(child);

        // Insert _ast row for every named node (both dirs and leaves)
        insert_ast(
            conn,
            &id,
            source_id,
            kind,
            child.start_byte(),
            child.end_byte(),
            child.start_position().row,
            child.start_position().column,
            child.end_position().row,
            child.end_position().column,
        )?;

        if has_named_children {
            // Directory node — empty record
            insert_node(conn, &id, parent_id, &name, 1, 0, mtime, "")?;
            let mut sub_cursor = child.walk();
            walk_children(content, &mut sub_cursor, &id, mtime, conn, source_id)?;
        } else {
            // Leaf (file) node — record is plain text from source
            let text = child.utf8_text(content).unwrap_or("");
            insert_node(
                conn,
                &id,
                parent_id,
                &name,
                0,
                text.len() as i64,
                mtime,
                text,
            )?;
        }
    }

    Ok(())
}

/// Check if a node has any named children.
fn has_named_child(node: &tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    fn count_nodes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap()
    }

    fn get_node(conn: &Connection, id: &str) -> (String, i32, String) {
        conn.query_row(
            "SELECT name, kind, record FROM nodes WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn all_ids(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT id FROM nodes ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    #[cfg(feature = "html")]
    fn html_lang() -> Language {
        tree_sitter_html::LANGUAGE.into()
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_simple_element() {
        let conn = open_mem();
        let src = b"<p>Hello</p>";
        project_ast(src, html_lang(), &conn).unwrap();

        assert!(count_nodes(&conn) >= 5, "expected at least 5 nodes");

        // Root exists
        let (name, kind, _) = get_node(&conn, "");
        assert_eq!(name, "");
        assert_eq!(kind, 1);
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_sibling_disambiguation() {
        let conn = open_mem();
        let src = b"<ul><li>A</li><li>B</li></ul>";
        project_ast(src, html_lang(), &conn).unwrap();

        let ids = all_ids(&conn);
        let has_li0 = ids.iter().any(|id| id.contains("element_0"));
        let has_li1 = ids.iter().any(|id| id.contains("element_1"));
        assert!(has_li0, "expected element_0 disambiguation, ids: {ids:?}");
        assert!(has_li1, "expected element_1 disambiguation, ids: {ids:?}");
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_attributes() {
        let conn = open_mem();
        let src = b"<div id=\"x\">text</div>";
        project_ast(src, html_lang(), &conn).unwrap();

        let ids = all_ids(&conn);
        let has_attr = ids.iter().any(|id| id.contains("attribute"));
        assert!(has_attr, "expected attribute node, ids: {ids:?}");
    }

    #[cfg(feature = "html")]
    #[test]
    fn record_is_plain_text() {
        let conn = open_mem();
        let src = b"<p>Hello</p>";
        project_ast(src, html_lang(), &conn).unwrap();

        // Leaf "text" node record should be plain text, not JSON
        let record: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE name = 'text' AND kind = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(record, "Hello");
    }

    #[cfg(feature = "html")]
    #[test]
    fn dir_record_is_empty() {
        let conn = open_mem();
        let src = b"<p>Hello</p>";
        project_ast(src, html_lang(), &conn).unwrap();

        // Dir "element" node record should be empty
        let record: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE name = 'element' AND kind = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(record, "");
    }

    #[cfg(feature = "html")]
    #[test]
    fn ast_table_populated() {
        let conn = open_mem();
        let src = b"<p>Hello</p>";
        project_ast(src, html_lang(), &conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "expected _ast rows");

        // Check that a leaf node has correct byte range
        let (start_byte, end_byte): (i64, i64) = conn
            .query_row(
                "SELECT start_byte, end_byte FROM _ast WHERE node_id = \
                 (SELECT id FROM nodes WHERE name = 'text' AND kind = 0 LIMIT 1)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // "Hello" is at bytes 3..8 in "<p>Hello</p>"
        assert_eq!(start_byte, 3);
        assert_eq!(end_byte, 8);
    }

    #[cfg(feature = "html")]
    #[test]
    fn source_table_populated() {
        let conn = open_mem();
        let src = b"<p>Hello</p>";
        project_ast(src, html_lang(), &conn).unwrap();

        let (language, content): (String, Vec<u8>) = conn
            .query_row("SELECT language, content FROM _source LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(language, "unknown"); // default from project_ast wrapper
        assert_eq!(content, src);
    }

    #[cfg(feature = "html")]
    #[test]
    fn all_ids_unique() {
        let conn = open_mem();
        let src = b"<html><head><title>T</title></head><body><div><p>A</p><p>B</p></div><ul><li>1</li><li>2</li><li>3</li></ul></body></html>";
        project_ast(src, html_lang(), &conn).unwrap();

        let ids = all_ids(&conn);
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(id.clone()), "duplicate id: {id}");
        }
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn markdown_heading() {
        let conn = open_mem();
        let src = b"# Title\n\nBody text here.\n";
        let lang: Language = tree_sitter_md::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let has_heading = ids.iter().any(|id| id.contains("atx_heading"));
        let has_paragraph = ids.iter().any(|id| id.contains("paragraph"));
        assert!(has_heading, "expected atx_heading node, ids: {ids:?}");
        assert!(has_paragraph, "expected paragraph node, ids: {ids:?}");
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_object_structure() {
        let conn = open_mem();
        let src = br#"{"name": "leyline", "version": 1}"#;
        let lang: Language = tree_sitter_json::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let has_pair = ids.iter().any(|id| id.contains("pair"));
        let has_string = ids.iter().any(|id| id.contains("string"));
        assert!(has_pair, "expected pair node, ids: {ids:?}");
        assert!(has_string, "expected string node, ids: {ids:?}");
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_array_disambiguation() {
        let conn = open_mem();
        let src = br#"[{"a":1},{"b":2},{"c":3}]"#;
        let lang: Language = tree_sitter_json::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let has_obj0 = ids.iter().any(|id| id.contains("object_0"));
        let has_obj2 = ids.iter().any(|id| id.contains("object_2"));
        assert!(has_obj0, "expected object_0, ids: {ids:?}");
        assert!(has_obj2, "expected object_2, ids: {ids:?}");
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_mapping_structure() {
        let conn = open_mem();
        let src = b"name: leyline\nversion: 1\n";
        let lang: Language = tree_sitter_yaml::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let has_block_mapping = ids.iter().any(|id| id.contains("block_mapping"));
        assert!(
            has_block_mapping,
            "expected block_mapping node, ids: {ids:?}"
        );
        assert!(
            count_nodes(&conn) >= 3,
            "expected at least 3 nodes for yaml mapping"
        );
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_list_structure() {
        let conn = open_mem();
        let src = b"items:\n  - one\n  - two\n  - three\n";
        let lang: Language = tree_sitter_yaml::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let has_block_sequence = ids.iter().any(|id| id.contains("block_sequence"));
        assert!(
            has_block_sequence,
            "expected block_sequence node, ids: {ids:?}"
        );
    }

    // --- Edge case tests ---

    #[cfg(feature = "html")]
    #[test]
    fn empty_input() {
        let conn = open_mem();
        project_ast(b"", html_lang(), &conn).unwrap();
        assert!(count_nodes(&conn) >= 1);
    }

    #[cfg(feature = "html")]
    #[test]
    fn malformed_html() {
        let conn = open_mem();
        let src = b"<div><p>unclosed<span>nested</div>";
        project_ast(src, html_lang(), &conn).unwrap();
        assert!(count_nodes(&conn) >= 1);
        let ids = all_ids(&conn);
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(
                seen.insert(id.clone()),
                "duplicate id in malformed html: {id}"
            );
        }
    }

    #[cfg(feature = "html")]
    #[test]
    fn binary_garbage_input() {
        let conn = open_mem();
        let src: Vec<u8> = (0..256).map(|b| b as u8).collect();
        project_ast(&src, html_lang(), &conn).unwrap();
        assert!(count_nodes(&conn) >= 1);
    }

    #[cfg(feature = "html")]
    #[test]
    fn deeply_nested_html() {
        let conn = open_mem();
        let open: String = (0..100).map(|_| "<div>").collect();
        let close: String = (0..100).map(|_| "</div>").collect();
        let src = format!("{open}leaf{close}");
        project_ast(src.as_bytes(), html_lang(), &conn).unwrap();

        let ids = all_ids(&conn);
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(
                seen.insert(id.clone()),
                "duplicate id in deep nesting: {id}"
            );
        }
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_malformed_input() {
        let conn = open_mem();
        let src = br#"{"key": "no closing brace"#;
        let lang: Language = tree_sitter_json::LANGUAGE.into();
        project_ast(src, lang, &conn).unwrap();
        assert!(count_nodes(&conn) >= 1);
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_deeply_nested_object() {
        // Scale pin for registry-repo data: helm chart values, JSON Schema
        // documents, and OpenAPI specs commonly nest 50–100 levels deep.
        // Pin that 256-level nesting (~2.5x typical) parses without
        // stack overflow OR runaway recursion in the projection walk.
        // If this regresses, registry-repo ingest crashes silently on
        // pathological-but-legal data.
        let conn = open_mem();
        let depth = 256;
        let mut src = String::new();
        for _ in 0..depth {
            src.push_str("{\"k\":");
        }
        src.push_str("42");
        for _ in 0..depth {
            src.push('}');
        }
        let lang: Language = tree_sitter_json::LANGUAGE.into();
        project_ast(src.as_bytes(), lang, &conn).unwrap();
        // At least one node per nesting level + the leaf number.
        assert!(
            count_nodes(&conn) >= depth as i64,
            "expected ≥{depth} nodes for {depth}-deep nesting, got {}",
            count_nodes(&conn),
        );
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_deeply_nested_mapping() {
        // Scale pin parallel to json_deeply_nested_object. YAML block
        // mappings nest naturally in helm charts (values.yaml under
        // multi-tenant overlays).
        let conn = open_mem();
        let depth = 100;
        let mut src = String::new();
        for i in 0..depth {
            // Use 2-space indent per level. YAML can't reasonably go to
            // 256 levels because the indentation alone would consume
            // 512 bytes per leaf line; 100 is comfortably above the
            // ~30-50 levels real registry repos hit.
            for _ in 0..i {
                src.push_str("  ");
            }
            src.push_str(&format!("k{i}:\n"));
        }
        // Final value at the deepest level.
        for _ in 0..depth {
            src.push_str("  ");
        }
        src.push_str("leaf: ok\n");

        let lang: Language = tree_sitter_yaml::LANGUAGE.into();
        project_ast(src.as_bytes(), lang, &conn).unwrap();
        assert!(
            count_nodes(&conn) >= depth as i64,
            "expected ≥{depth} nodes for {depth}-deep nesting, got {}",
            count_nodes(&conn),
        );
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_wide_array() {
        // Scale pin: arrays with thousands of siblings (e.g. helm values
        // overrides, Kubernetes resource lists). The projection walk
        // iterates siblings linearly; pin that 5000 siblings stays
        // tractable + every sibling gets a unique id.
        let conn = open_mem();
        let n = 5000;
        let elements: Vec<String> = (0..n).map(|i| i.to_string()).collect();
        let src = format!("[{}]", elements.join(","));

        let lang: Language = tree_sitter_json::LANGUAGE.into();
        project_ast(src.as_bytes(), lang, &conn).unwrap();

        let ids = all_ids(&conn);
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(
                seen.insert(id.clone()),
                "duplicate id in wide array: {id}",
            );
        }
        assert!(
            ids.len() >= n,
            "expected ≥{n} unique ids for {n}-element array, got {}",
            ids.len(),
        );
    }
}
