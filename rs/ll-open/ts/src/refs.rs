//! Cross-reference extraction from tree-sitter AST nodes.
//!
//! Language-specific extractors produce `ExtractedRef` values.
//! The caller decides how to store them (SQLite, vector, etc.).

use tree_sitter::Node;

/// A single extracted reference, definition, or import.
///
/// Universal across languages — Go, Python, JS, etc. all produce these.
/// The extraction function is language-specific; the data type is not.
#[derive(Debug, Clone)]
pub enum ExtractedRef {
    /// A function/method/type definition.
    Def {
        token: String,
        node_id: String,
        source_id: String,
    },
    /// A call-site reference.
    Ref {
        token: String,
        node_id: String,
        source_id: String,
    },
    /// An import alias→path mapping.
    Import {
        alias: String,
        path: String,
        source_id: String,
    },
}

/// Insert extracted refs into SQLite tables.
///
/// Universal — works with output from any language extractor.
pub fn insert_extracted_refs(
    conn: &rusqlite::Connection,
    refs: &[ExtractedRef],
) -> anyhow::Result<()> {
    for r in refs {
        match r {
            ExtractedRef::Def {
                token,
                node_id,
                source_id,
            } => crate::schema::insert_def(conn, token, node_id, source_id)?,
            ExtractedRef::Ref {
                token,
                node_id,
                source_id,
            } => crate::schema::insert_ref(conn, token, node_id, source_id)?,
            ExtractedRef::Import {
                alias,
                path,
                source_id,
            } => crate::schema::insert_import(conn, alias, path, source_id)?,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Language dispatcher (factory)
// ---------------------------------------------------------------------------

/// Extract refs/defs/imports from an AST node, dispatching by language.
///
/// Unsupported languages return an empty vec (no refs, no error).
/// Add new languages by adding a match arm here + an `extract_<lang>` function.
pub fn extract_refs(
    node: &tree_sitter::Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    language: crate::languages::TsLanguage,
) -> Vec<ExtractedRef> {
    match language {
        #[cfg(feature = "go")]
        crate::languages::TsLanguage::Go => extract_go(node, source, node_id, source_id),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Go extractor
// ---------------------------------------------------------------------------

/// Extract Go definitions, call references, and imports from a single AST node.
///
/// Pure data — no database access, safe for parallel use.
///
/// Node kinds handled:
/// - `function_declaration`, `method_declaration`, `type_spec` → Def
/// - `call_expression` → Ref (simple) or Ref (qualified: "pkg.Func")
/// - `import_spec` → Import
pub fn extract_go(node: &Node, source: &[u8], node_id: &str, source_id: &str) -> Vec<ExtractedRef> {
    let mut out = Vec::new();

    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(token) = name_node.utf8_text(source) {
                    if !token.is_empty() {
                        out.push(ExtractedRef::Def {
                            token: token.to_string(),
                            node_id: node_id.to_string(),
                            source_id: source_id.to_string(),
                        });
                    }
                }
            }
        }
        "type_spec" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(token) = name_node.utf8_text(source) {
                    if !token.is_empty() {
                        out.push(ExtractedRef::Def {
                            token: token.to_string(),
                            node_id: node_id.to_string(),
                            source_id: source_id.to_string(),
                        });
                    }
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    "identifier" => {
                        if let Ok(token) = func_node.utf8_text(source) {
                            if !token.is_empty() {
                                out.push(ExtractedRef::Ref {
                                    token: token.to_string(),
                                    node_id: node_id.to_string(),
                                    source_id: source_id.to_string(),
                                });
                            }
                        }
                    }
                    "selector_expression" => {
                        let pkg = func_node
                            .child_by_field_name("operand")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let func = func_node
                            .child_by_field_name("field")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if !func.is_empty() {
                            if !pkg.is_empty() {
                                out.push(ExtractedRef::Ref {
                                    token: format!("{pkg}.{func}"),
                                    node_id: node_id.to_string(),
                                    source_id: source_id.to_string(),
                                });
                            }
                            out.push(ExtractedRef::Ref {
                                token: func.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_spec" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .trim_matches('"');

            if !path.is_empty() {
                let alias = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");

                let alias = if alias.is_empty() || alias == "." {
                    path.rsplit('/').next().unwrap_or(path)
                } else {
                    alias
                };

                out.push(ExtractedRef::Import {
                    alias: alias.to_string(),
                    path: path.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        _ => {}
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(feature = "go")]
mod tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

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

    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_go(&child, src, &id, "test.go");
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT token FROM node_defs ORDER BY token").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT token FROM node_refs ORDER BY token").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn.prepare("SELECT alias, path FROM _imports ORDER BY path").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn extract_function_defs() {
        let src = b"package main\n\nfunc Add() {}\nfunc Sub() {}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(defs.contains(&"Add".to_string()));
        assert!(defs.contains(&"Sub".to_string()));
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn extract_call_refs() {
        let src = b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n\tAdd()\n}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(refs.contains(&"Add".to_string()));
        assert!(refs.contains(&"Println".to_string()));
        assert!(refs.contains(&"fmt.Println".to_string()));
    }

    #[test]
    fn extract_imports() {
        let src = b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(imports.contains(&("fmt".to_string(), "fmt".to_string())));
        assert!(imports.contains(&("auth".to_string(), "github.com/foo/auth".to_string())));
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn extract_method_and_type_defs() {
        let src = b"package main\n\ntype Server struct{}\n\nfunc (s *Server) Start() {}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(defs.contains(&"Server".to_string()));
        assert!(defs.contains(&"Start".to_string()));
    }
}
