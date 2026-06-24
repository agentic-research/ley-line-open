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
//
// `#[allow(unused_variables)]`: every match arm is feature-gated, so when
// no language with a refs extractor is enabled the parameters are unused.
// They're load-bearing when any extractor feature is on.
#[allow(unused_variables)]
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
        #[cfg(feature = "rust")]
        crate::languages::TsLanguage::Rust => extract_rust(node, source, node_id, source_id),
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
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(token) = name_node.utf8_text(source)
                && !token.is_empty()
            {
                out.push(ExtractedRef::Def {
                    token: token.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        "type_spec" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(token) = name_node.utf8_text(source)
                && !token.is_empty()
            {
                out.push(ExtractedRef::Def {
                    token: token.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    "identifier" => {
                        if let Ok(token) = func_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                            });
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
// Rust extractor
// ---------------------------------------------------------------------------

/// Extract Rust definitions, call references, macro invocations, and `use`
/// imports from a single AST node. Pure data — no DB access.
///
/// Node kinds handled (tree-sitter-rust grammar):
/// - `function_item`, `struct_item`, `enum_item`, `union_item`,
///   `trait_item`, `type_item`, `mod_item`, `const_item`, `static_item`
///   → Def (uses `name` field)
/// - `call_expression`:
///     - `function: identifier`           → Ref (bare token)
///     - `function: field_expression`     → Ref (method name from `field`)
///     - `function: scoped_identifier`    → Ref (qualified `pkg::func` + bare `func`)
/// - `macro_invocation` → Ref (`macro` field; includes the `!` is dropped)
/// - `use_declaration` → Import. Handles bare, scoped, aliased, and
///   list/scoped-list use trees. Wildcards are skipped (no addressable
///   alias). Nested `use_list` cases recurse via the walker, not here.
///
/// Closures (`closure_expression`) are intentionally NOT matched — they're
/// anonymous, no stable token.
#[cfg(feature = "rust")]
pub fn extract_rust(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
) -> Vec<ExtractedRef> {
    let mut out = Vec::new();

    match node.kind() {
        // ── Definitions: anything with a `name` field that introduces a
        // top-level binding the rest of the codebase can reference.
        // `function_signature_item` is the bodyless form used in traits
        // (`fn x(&self);`) and `extern` blocks. Same `name` field.
        "function_item"
        | "function_signature_item"
        | "struct_item"
        | "enum_item"
        | "union_item"
        | "trait_item"
        | "type_item"
        | "mod_item"
        | "const_item"
        | "static_item" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(token) = name_node.utf8_text(source)
                && !token.is_empty()
            {
                out.push(ExtractedRef::Def {
                    token: token.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }

        // ── Call references: tree-sitter-rust's `call_expression` always
        // has a `function` field; we branch on that field's kind.
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    // Bare call: `foo()`.
                    "identifier" => {
                        if let Ok(token) = func_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    // Method-like: `obj.method()`. The receiver isn't a
                    // ref (it's a value), so we emit only the field name.
                    "field_expression" => {
                        if let Some(field_node) = func_node.child_by_field_name("field")
                            && let Ok(token) = field_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    // Qualified: `mod::func()`. Emit both the qualified
                    // form ("module::func") and the bare form ("func") so
                    // a downstream resolver can match either.
                    "scoped_identifier" => {
                        let qualified = func_node.utf8_text(source).unwrap_or("");
                        let bare = func_node
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if !bare.is_empty() {
                            if !qualified.is_empty() {
                                out.push(ExtractedRef::Ref {
                                    token: qualified.to_string(),
                                    node_id: node_id.to_string(),
                                    source_id: source_id.to_string(),
                                });
                            }
                            out.push(ExtractedRef::Ref {
                                token: bare.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // ── Macro invocations: `println!`, `vec!`, etc.
        "macro_invocation" => {
            if let Some(macro_node) = node.child_by_field_name("macro") {
                // `macro` may be `identifier` or `scoped_identifier`.
                let token = match macro_node.kind() {
                    "scoped_identifier" => macro_node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or(""),
                    _ => macro_node.utf8_text(source).unwrap_or(""),
                };
                if !token.is_empty() {
                    out.push(ExtractedRef::Ref {
                        token: token.to_string(),
                        node_id: node_id.to_string(),
                        source_id: source_id.to_string(),
                    });
                }
            }
        }

        // ── Imports: `use_declaration` wraps a single `argument` tree.
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                collect_use_imports(arg, source, source_id, &mut out);
            }
        }

        _ => {}
    }

    out
}

/// Recursively flatten a `use` argument into `ExtractedRef::Import`
/// entries. Tree-sitter-rust models the `use` tree as nested
/// `scoped_identifier` / `use_as_clause` / `use_list` / `scoped_use_list`
/// nodes; the recursion mirrors that shape.
///
/// Wildcards (`use foo::*;`) are skipped — no stable alias to attach.
#[cfg(feature = "rust")]
fn collect_use_imports(
    node: Node<'_>,
    source: &[u8],
    source_id: &str,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        // Bare `use foo;`
        "identifier" => {
            if let Ok(name) = node.utf8_text(source)
                && !name.is_empty()
            {
                out.push(ExtractedRef::Import {
                    alias: name.to_string(),
                    path: name.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        // `use foo::bar;` — full path is the node text, alias = last
        // segment.
        "scoped_identifier" => {
            let path = node.utf8_text(source).unwrap_or("");
            let alias = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if !path.is_empty() && !alias.is_empty() {
                out.push(ExtractedRef::Import {
                    alias: alias.to_string(),
                    path: path.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        // `use foo::bar as baz;` — explicit alias.
        "use_as_clause" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let alias = node
                .child_by_field_name("alias")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if !path.is_empty() && !alias.is_empty() {
                out.push(ExtractedRef::Import {
                    alias: alias.to_string(),
                    path: path.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        // `use foo::{a, b};` — list children are individual use trees
        // sharing the `foo::` prefix. tree-sitter-rust emits these as a
        // `scoped_use_list` node with `path: foo` and `list: use_list`.
        "scoped_use_list" => {
            let path_prefix = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.named_children(&mut cursor) {
                    collect_use_list_child(child, source, source_id, path_prefix, out);
                }
            }
        }
        // `use {a, b};` (rare: top-level un-prefixed list).
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_use_list_child(child, source, source_id, "", out);
            }
        }
        // `use foo::*;` — intentionally skipped (no addressable alias).
        "use_wildcard" => {}
        _ => {}
    }
}

/// Helper for items inside a `use_list` — the leaf may be a bare ident
/// (`a` → `foo::a`), a scoped ident (`a::b` → `foo::a::b`), or an alias
/// clause (`a as renamed` → `foo::a` with alias=`renamed`).
#[cfg(feature = "rust")]
fn collect_use_list_child(
    node: Node<'_>,
    source: &[u8],
    source_id: &str,
    path_prefix: &str,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(source).unwrap_or("");
            if name.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                name.to_string()
            } else {
                format!("{path_prefix}::{name}")
            };
            out.push(ExtractedRef::Import {
                alias: name.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        "scoped_identifier" => {
            let leaf_path = node.utf8_text(source).unwrap_or("");
            let alias = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if leaf_path.is_empty() || alias.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                leaf_path.to_string()
            } else {
                format!("{path_prefix}::{leaf_path}")
            };
            out.push(ExtractedRef::Import {
                alias: alias.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        "use_as_clause" => {
            let leaf_path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let alias = node
                .child_by_field_name("alias")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if leaf_path.is_empty() || alias.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                leaf_path.to_string()
            } else {
                format!("{path_prefix}::{leaf_path}")
            };
            out.push(ExtractedRef::Import {
                alias: alias.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        // Nested lists: `use foo::{bar::{a, b}};`
        "scoped_use_list" | "use_list" => {
            collect_use_imports(node, source, source_id, out);
        }
        // self / wildcard inside a list — skip.
        _ => {}
    }
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
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
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

#[cfg(test)]
#[cfg(feature = "rust")]
mod rust_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_rust(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
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
                    let refs = extract_rust(&child, src, &id, "test.rs");
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
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_function_and_method_defs() {
        let src =
            b"fn add() {}\n\nfn sub() {}\n\nstruct Server;\n\nimpl Server { fn start(&self) {} }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        // Bare functions + impl method + struct.
        assert!(defs.contains(&"add".to_string()), "missing add: {defs:?}");
        assert!(defs.contains(&"sub".to_string()), "missing sub: {defs:?}");
        assert!(
            defs.contains(&"start".to_string()),
            "missing start: {defs:?}"
        );
        assert!(
            defs.contains(&"Server".to_string()),
            "missing Server: {defs:?}"
        );
    }

    #[test]
    fn extract_type_kind_defs() {
        let src = b"struct S;\nenum E { A, B }\ntrait T { fn x(&self); }\ntype Alias = u32;\nconst K: u32 = 1;\nstatic S2: u32 = 2;\nmod m { fn inner() {} }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in &["S", "E", "T", "Alias", "K", "S2", "m", "x", "inner"] {
            assert!(defs.contains(&want.to_string()), "missing {want}: {defs:?}");
        }
    }

    #[test]
    fn extract_call_refs_bare_and_method_and_scoped() {
        let src = b"fn main() { foo(); obj.bar(); std::process::exit(0); }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        // Bare call.
        assert!(refs.contains(&"foo".to_string()), "missing foo: {refs:?}");
        // Method call (field_expression's `field`).
        assert!(refs.contains(&"bar".to_string()), "missing bar: {refs:?}");
        // Scoped call: both the qualified and bare forms.
        assert!(
            refs.contains(&"exit".to_string()),
            "missing bare exit: {refs:?}"
        );
        assert!(
            refs.contains(&"std::process::exit".to_string()),
            "missing qualified: {refs:?}"
        );
    }

    #[test]
    fn extract_macro_invocations() {
        let src = b"fn main() { println!(\"hi\"); vec![1,2,3]; std::format!(\"{}\", 1); }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"println".to_string()),
            "missing println: {refs:?}"
        );
        assert!(refs.contains(&"vec".to_string()), "missing vec: {refs:?}");
        assert!(
            refs.contains(&"format".to_string()),
            "missing format: {refs:?}"
        );
    }

    #[test]
    fn extract_use_bare_scoped_and_alias() {
        let src = b"use foo;\nuse std::collections::HashMap;\nuse std::io as io_mod;\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        // Bare.
        assert!(
            imports.contains(&("foo".to_string(), "foo".to_string())),
            "missing bare: {imports:?}"
        );
        // Scoped — alias is last segment.
        assert!(
            imports.contains(&(
                "HashMap".to_string(),
                "std::collections::HashMap".to_string()
            )),
            "missing scoped: {imports:?}"
        );
        // Aliased.
        assert!(
            imports.contains(&("io_mod".to_string(), "std::io".to_string())),
            "missing alias: {imports:?}"
        );
    }

    #[test]
    fn extract_use_list_expands_each_leaf() {
        let src = b"use std::collections::{HashMap, HashSet, BTreeMap as Tree};\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&(
                "HashMap".to_string(),
                "std::collections::HashMap".to_string()
            )),
            "missing HashMap from list: {imports:?}"
        );
        assert!(
            imports.contains(&(
                "HashSet".to_string(),
                "std::collections::HashSet".to_string()
            )),
            "missing HashSet from list: {imports:?}"
        );
        assert!(
            imports.contains(&("Tree".to_string(), "std::collections::BTreeMap".to_string())),
            "missing aliased BTreeMap from list: {imports:?}"
        );
    }

    #[test]
    fn extract_skips_wildcard_use() {
        let src = b"use foo::*;\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        // Wildcards have no addressable alias — extractor must drop them.
        assert!(
            imports.is_empty(),
            "wildcard must not produce import: {imports:?}"
        );
    }
}
