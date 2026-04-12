//! Go ref extraction — definitions, call references, and imports from tree-sitter AST nodes.
//!
//! Called for every named node during AST traversal. Inspects `node.kind()` and
//! populates `node_defs`, `node_refs`, and `_imports` tables for Go source files.

use anyhow::Result;
use rusqlite::Connection;
use tree_sitter::Node;

use crate::schema::{insert_def, insert_import, insert_ref};

/// Extract Go definitions, call references, and imports from a single AST node.
///
/// Called for EVERY named node during `walk_children()`. Inspects `node.kind()`
/// and only acts on relevant kinds:
///
/// - **Definitions** (`function_declaration`, `method_declaration`, `type_spec`)
///   → inserted into `node_defs`
/// - **Call references** (`call_expression`) → inserted into `node_refs`
///   - Simple calls: `Add()` → ref "Add"
///   - Qualified calls: `fmt.Println()` → refs "fmt.Println" and "Println"
/// - **Imports** (`import_spec`) → inserted into `_imports`
pub fn extract_go_refs(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    conn: &Connection,
) -> Result<()> {
    match node.kind() {
        // -----------------------------------------------------------------
        // Definitions
        // -----------------------------------------------------------------
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let token = name_node.utf8_text(source).unwrap_or("");
                if !token.is_empty() {
                    insert_def(conn, token, node_id, source_id)?;
                }
            }
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let token = name_node.utf8_text(source).unwrap_or("");
                if !token.is_empty() {
                    insert_def(conn, token, node_id, source_id)?;
                }
            }
        }
        "type_spec" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let token = name_node.utf8_text(source).unwrap_or("");
                if !token.is_empty() {
                    insert_def(conn, token, node_id, source_id)?;
                }
            }
        }

        // -----------------------------------------------------------------
        // Call references
        // -----------------------------------------------------------------
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    "identifier" => {
                        // Simple call: Add()
                        let token = func_node.utf8_text(source).unwrap_or("");
                        if !token.is_empty() {
                            insert_ref(conn, token, node_id, source_id)?;
                        }
                    }
                    "selector_expression" => {
                        // Qualified call: fmt.Println()
                        let operand = func_node.child_by_field_name("operand");
                        let field = func_node.child_by_field_name("field");

                        if let (Some(op), Some(f)) = (operand, field) {
                            let pkg = op.utf8_text(source).unwrap_or("");
                            let func = f.utf8_text(source).unwrap_or("");
                            if !pkg.is_empty() && !func.is_empty() {
                                let qualified = format!("{pkg}.{func}");
                                insert_ref(conn, &qualified, node_id, source_id)?;
                                insert_ref(conn, func, node_id, source_id)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // -----------------------------------------------------------------
        // Imports
        // -----------------------------------------------------------------
        "import_spec" => {
            if let Some(path_node) = node.child_by_field_name("path") {
                let raw_path = path_node.utf8_text(source).unwrap_or("");
                // Strip surrounding quotes
                let path = raw_path.trim_matches('"');

                let alias = if let Some(name_node) = node.child_by_field_name("name") {
                    name_node.utf8_text(source).unwrap_or("").to_string()
                } else {
                    // Default alias = last path segment
                    path.rsplit('/').next().unwrap_or(path).to_string()
                };

                if !alias.is_empty() && !path.is_empty() {
                    insert_import(conn, &alias, path, source_id)?;
                }
            }
        }

        // All other node kinds — nothing to do
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
#[cfg(feature = "go")]
mod tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use tree_sitter::Parser;

    /// Set up an in-memory DB and parse Go source.
    fn parse_go(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    /// Recursively walk all named children, calling extract_go_refs on each.
    fn walk_all(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let _ = extract_go_refs(&child, src, &id, "test.go", conn);
                    walk_all(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Collect all tokens from node_defs.
    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    /// Collect all tokens from node_refs.
    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    /// Collect all (alias, path) pairs from _imports.
    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<(String, String)>, _>>()
            .unwrap()
    }

    #[test]
    fn extract_function_defs() {
        let src = b"package main\n\nfunc Add() {}\nfunc Sub() {}\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();
        walk_all(root, src, &conn, "");

        let defs = all_defs(&conn);
        assert!(defs.contains(&"Add".to_string()), "expected Add def, got: {defs:?}");
        assert!(defs.contains(&"Sub".to_string()), "expected Sub def, got: {defs:?}");
        assert_eq!(defs.len(), 2, "expected exactly 2 defs, got: {defs:?}");
    }

    #[test]
    fn extract_call_refs() {
        let src = b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n\tAdd()\n}\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();
        walk_all(root, src, &conn, "");

        let refs = all_refs(&conn);
        assert!(refs.contains(&"Add".to_string()), "expected Add ref, got: {refs:?}");
        assert!(
            refs.contains(&"Println".to_string()),
            "expected Println ref, got: {refs:?}"
        );
        assert!(
            refs.contains(&"fmt.Println".to_string()),
            "expected fmt.Println ref, got: {refs:?}"
        );
    }

    #[test]
    fn extract_imports() {
        let src = b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();
        walk_all(root, src, &conn, "");

        let imports = all_imports(&conn);
        assert!(
            imports.contains(&("fmt".to_string(), "fmt".to_string())),
            "expected fmt import, got: {imports:?}"
        );
        assert!(
            imports.contains(&("auth".to_string(), "github.com/foo/auth".to_string())),
            "expected auth import, got: {imports:?}"
        );
        assert_eq!(imports.len(), 2, "expected exactly 2 imports, got: {imports:?}");
    }

    #[test]
    fn extract_method_and_type_defs() {
        let src = b"package main\n\ntype Server struct{}\n\nfunc (s *Server) Start() {}\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();
        walk_all(root, src, &conn, "");

        let defs = all_defs(&conn);
        assert!(
            defs.contains(&"Server".to_string()),
            "expected Server def, got: {defs:?}"
        );
        assert!(
            defs.contains(&"Start".to_string()),
            "expected Start def, got: {defs:?}"
        );
    }
}
