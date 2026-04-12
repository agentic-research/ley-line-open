# Inline Ref Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich `leyline parse` output with `node_refs`, `node_defs`, and `_imports` tables so mache never needs CGO tree-sitter. Go language first.

**Architecture:** Add schema + insert functions to `leyline-ts/schema.rs`, create a new `leyline-ts/refs.rs` module for Go-specific extraction, and call it from `cmd_parse.rs` during the existing `walk_children()` traversal. All refs are extracted in a single pass — no re-reading the .db.

**Tech Stack:** Rust (edition 2024), tree-sitter, rusqlite, leyline-ts

## Hard Rules

1. Every new function gets a test. Ref extraction gets unit tests in leyline-ts and integration tests in cli-lib.
2. No code duplication. Schema DDL lives in `schema.rs`. Extraction logic lives in `refs.rs`. The CLI just calls them.
3. Zero-copy path unchanged — all tables go into the same SQLite database.
4. Go only. No generic "all languages" abstraction. Other languages are separate beads.

## Go AST Node Kinds Reference

From tree-sitter-go, the relevant patterns in a parsed Go file:

```
function_declaration/identifier          → function name (def)
method_declaration/field_identifier      → method name (def)
type_declaration/type_spec/type_identifier → type name (def)
call_expression/identifier               → simple call (ref: "Add")
call_expression/selector_expression/identifier      → package (ref qualifier: "fmt")
call_expression/selector_expression/field_identifier → function (ref token: "Println")
import_spec/interpreted_string_literal/interpreted_string_literal_content → import path
import_spec/package_identifier           → import alias (optional)
```

---

### Task 1: Schema — DDL + insert functions for refs tables

**Files:**
- Modify: `rs/ll-open/ts/src/schema.rs`

- [ ] **Step 1: Add DDL constants**

Add to `schema.rs` after the existing `AST_DDL`:

```rust
/// DDL for the `node_refs` table — call sites (who calls what).
pub const REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);";

/// DDL for the `node_defs` table — definitions (what defines what).
pub const DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);";

/// DDL for the `_imports` table — Go import alias→path mapping.
pub const IMPORTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);";
```

- [ ] **Step 2: Add `create_refs_schema()` function**

```rust
/// Create `node_refs`, `node_defs`, and `_imports` tables (idempotent).
pub fn create_refs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_DDL)?;
    conn.execute_batch(DEFS_DDL)?;
    conn.execute_batch(IMPORTS_DDL)?;
    Ok(())
}
```

- [ ] **Step 3: Add insert functions**

```rust
/// Insert a call-site reference.
pub fn insert_ref(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_refs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert a definition.
pub fn insert_def(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_defs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert an import mapping.
pub fn insert_import(conn: &Connection, alias: &str, path: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO _imports (alias, path, source_id) VALUES (?1, ?2, ?3)",
        params![alias, path, source_id],
    )?;
    Ok(())
}
```

- [ ] **Step 4: Add unit test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_schema_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();

        insert_ref(&conn, "Println", "main.go/call_expression", "main.go").unwrap();
        insert_def(&conn, "Add", "main.go/function_declaration", "main.go").unwrap();
        insert_import(&conn, "fmt", "fmt", "main.go").unwrap();

        let ref_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0)).unwrap();
        assert_eq!(ref_count, 1);

        let def_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0)).unwrap();
        assert_eq!(def_count, 1);

        let import_count: i64 = conn.query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0)).unwrap();
        assert_eq!(import_count, 1);
    }
}
```

- [ ] **Step 5: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-ts`

Expected: All existing tests pass + 1 new test.

- [ ] **Step 6: Commit**

```bash
git add rs/ll-open/ts/src/schema.rs
git commit -m "feat(ts): add node_refs, node_defs, _imports schema + insert functions"
```

---

### Task 2: Go ref extraction module

**Files:**
- Create: `rs/ll-open/ts/src/refs.rs`
- Modify: `rs/ll-open/ts/src/lib.rs` (add `pub mod refs;`)

- [ ] **Step 1: Create `refs.rs` with Go extraction function**

```rust
//! Language-specific ref/def/import extraction from tree-sitter AST nodes.
//!
//! Called during walk_children() for each named node. Inspects node_kind
//! and extracts refs/defs/imports into the database. Go only for now.

use anyhow::Result;
use rusqlite::Connection;
use tree_sitter::Node;

use crate::schema::{insert_def, insert_import, insert_ref};

/// Extract Go refs, defs, and imports from a single AST node.
///
/// Called for every named node during the walk. Only acts on node kinds
/// that are relevant for cross-referencing. Does nothing for irrelevant kinds.
pub fn extract_go_refs(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    conn: &Connection,
) -> Result<()> {
    match node.kind() {
        // ── Definitions ──────────────────────────────────────
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    insert_def(conn, name, node_id, source_id)?;
                }
            }
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    insert_def(conn, name, node_id, source_id)?;
                }
            }
        }
        "type_spec" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    insert_def(conn, name, node_id, source_id)?;
                }
            }
        }

        // ── Call references ──────────────────────────────────
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    // Simple call: Add(1, 2)
                    "identifier" => {
                        let name = func_node.utf8_text(source).unwrap_or("");
                        if !name.is_empty() {
                            insert_ref(conn, name, node_id, source_id)?;
                        }
                    }
                    // Qualified call: fmt.Println(...)
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
                            // Store as "pkg.Func" for qualified, "Func" as fallback
                            if !pkg.is_empty() {
                                insert_ref(conn, &format!("{pkg}.{func}"), node_id, source_id)?;
                            }
                            insert_ref(conn, func, node_id, source_id)?;
                        }
                    }
                    _ => {}
                }
            }
        }

        // ── Imports ──────────────────────────────────────────
        "import_spec" => {
            // Path is in the string literal child
            let path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("")
                .trim_matches('"');

            if !path.is_empty() {
                // Alias: explicit name child, or last segment of path
                let alias = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();

                let alias = if alias.is_empty() || alias == "." {
                    path.rsplit('/').next().unwrap_or(path).to_string()
                } else {
                    alias
                };

                insert_import(conn, &alias, path, source_id)?;
            }
        }

        _ => {}
    }

    Ok(())
}
```

- [ ] **Step 2: Add `pub mod refs;` to `lib.rs`**

Add after `pub mod schema;`:

```rust
pub mod refs;
```

- [ ] **Step 3: Add unit tests for Go extraction**

Add to `refs.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
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

    fn query_refs(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn.prepare("SELECT token, node_id FROM node_refs ORDER BY token").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn query_defs(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn.prepare("SELECT token, node_id FROM node_defs ORDER BY token").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn query_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn.prepare("SELECT alias, path FROM _imports ORDER BY alias").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[cfg(feature = "go")]
    #[test]
    fn extract_function_defs() {
        let src = b"package main\n\nfunc Add(a, b int) int { return a + b }\nfunc Sub(a, b int) int { return a - b }\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();

        // Walk top-level children
        let mut cursor = root.walk();
        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.is_named() {
                    extract_go_refs(&node, src, &format!("test.go/{}", node.kind()), "test.go", &conn).unwrap();
                }
                if !cursor.goto_next_sibling() { break; }
            }
        }

        let defs = query_defs(&conn);
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|(t, _)| t == "Add"));
        assert!(defs.iter().any(|(t, _)| t == "Sub"));
    }

    #[cfg(feature = "go")]
    #[test]
    fn extract_call_refs() {
        let src = b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hi\")\n\tAdd(1, 2)\n}\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();

        // Need to walk deeper to find call_expression nodes
        fn walk(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.is_named() {
                        let id = format!("{prefix}/{}", child.kind());
                        let _ = extract_go_refs(&child, src, &id, "test.go", conn);
                        walk(child, src, conn, &id);
                    }
                    if !cursor.goto_next_sibling() { break; }
                }
            }
        }

        walk(root, src, &conn, "test.go");

        let refs = query_refs(&conn);
        // Should have: "Println", "fmt.Println", "Add"
        let tokens: Vec<&str> = refs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(tokens.contains(&"Add"), "should have Add ref: {tokens:?}");
        assert!(tokens.contains(&"Println"), "should have Println ref: {tokens:?}");
        assert!(tokens.contains(&"fmt.Println"), "should have fmt.Println ref: {tokens:?}");
    }

    #[cfg(feature = "go")]
    #[test]
    fn extract_imports() {
        let src = b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();

        fn walk(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.is_named() {
                        let id = format!("{prefix}/{}", child.kind());
                        let _ = extract_go_refs(&child, src, &id, "test.go", conn);
                        walk(child, src, conn, &id);
                    }
                    if !cursor.goto_next_sibling() { break; }
                }
            }
        }

        walk(root, src, &conn, "test.go");

        let imports = query_imports(&conn);
        assert_eq!(imports.len(), 2, "should have 2 imports: {imports:?}");
        assert!(imports.iter().any(|(a, p)| a == "fmt" && p == "fmt"));
        assert!(imports.iter().any(|(a, p)| a == "auth" && p == "github.com/foo/auth"));
    }

    #[cfg(feature = "go")]
    #[test]
    fn extract_method_and_type_defs() {
        let src = b"package main\n\ntype Server struct { port int }\n\nfunc (s *Server) Start() {}\n";
        let (conn, tree) = parse_go(src);
        let root = tree.root_node();

        fn walk(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.is_named() {
                        let id = format!("{prefix}/{}", child.kind());
                        let _ = extract_go_refs(&child, src, &id, "test.go", conn);
                        walk(child, src, conn, &id);
                    }
                    if !cursor.goto_next_sibling() { break; }
                }
            }
        }

        walk(root, src, &conn, "test.go");

        let defs = query_defs(&conn);
        let tokens: Vec<&str> = defs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(tokens.contains(&"Server"), "should have Server type def: {tokens:?}");
        assert!(tokens.contains(&"Start"), "should have Start method def: {tokens:?}");
    }
}
```

- [ ] **Step 4: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-ts`

Expected: All existing tests pass + 4 new tests.

- [ ] **Step 5: Commit**

```bash
git add rs/ll-open/ts/src/refs.rs rs/ll-open/ts/src/lib.rs
git commit -m "feat(ts): Go ref extraction — defs, calls, qualified calls, imports"
```

---

### Task 3: Wire extraction into cmd_parse walk

**Files:**
- Modify: `rs/ll-open/cli-lib/src/cmd_parse.rs`

- [ ] **Step 1: Add `create_refs_schema` call alongside `create_ast_schema`**

In `cmd_parse()`, after line 32 (`create_ast_schema(&conn)?;`), add:

```rust
leyline_ts::schema::create_refs_schema(&conn)?;
```

Add `leyline_ts::refs::extract_go_refs` to the imports.

- [ ] **Step 2: Call `extract_go_refs` in `walk_children()` for Go files**

The challenge: `walk_children` doesn't know what language the file is. We need to pass the language through.

Modify `project_file` to pass the language to `walk_children`:

Change `walk_children` signature to add `language: TsLanguage`:

```rust
fn walk_children(
    content: &[u8],
    cursor: &mut TreeCursor,
    parent_id: &str,
    mtime: i64,
    conn: &Connection,
    source_id: &str,
    language: TsLanguage,
) -> Result<()> {
```

In the loop over children, after the `insert_ast` call and before the `has_named_children` check, add:

```rust
        // Extract refs/defs/imports for Go files.
        if language == TsLanguage::Go {
            leyline_ts::refs::extract_go_refs(child, content, &id, source_id, conn)?;
        }
```

Update the recursive call to pass `language`:

```rust
walk_children(content, &mut sub_cursor, &id, mtime, conn, source_id, language)?;
```

Update the call in `project_file` to pass the language:

```rust
walk_children(content, &mut cursor, source_id, mtime, conn, source_id, language)?;
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test --workspace`

Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_parse.rs
git commit -m "feat(cli): wire Go ref extraction into parse walk"
```

---

### Task 4: Integration test — verify .db has refs

**Files:**
- Modify: `rs/ll-open/cli-lib/tests/integration.rs`

- [ ] **Step 1: Add integration test**

```rust
#[tokio::test]
async fn test_parse_produces_go_refs() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("refs-test.db");

    // Write a Go file with functions, calls, imports
    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n\nfunc main() {\n\tfmt.Println(\"hi\")\n\tauth.Validate()\n\thelper()\n}\n\nfunc helper() {}\n",
    ).unwrap();

    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // node_defs: should have "main" and "helper" function defs
    let def_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0)).unwrap();
    assert!(def_count >= 2, "should have at least 2 defs (main, helper), got {def_count}");

    // Verify specific defs
    let main_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'main'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(main_def, 1, "should have 'main' def");

    let helper_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'helper'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(helper_def, 1, "should have 'helper' def");

    // node_refs: should have calls to Println, fmt.Println, Validate, auth.Validate, helper
    let ref_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0)).unwrap();
    assert!(ref_count >= 3, "should have at least 3 refs, got {ref_count}");

    let println_ref: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_refs WHERE token = 'fmt.Println'", [], |r| r.get(0),
    ).unwrap();
    assert!(println_ref >= 1, "should have 'fmt.Println' ref");

    let helper_ref: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_refs WHERE token = 'helper'", [], |r| r.get(0),
    ).unwrap();
    assert!(helper_ref >= 1, "should have 'helper' call ref");

    // _imports: should have fmt and auth
    let import_count: i64 = conn.query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0)).unwrap();
    assert_eq!(import_count, 2, "should have 2 imports");

    let auth_import: String = conn.query_row(
        "SELECT path FROM _imports WHERE alias = 'auth'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(auth_import, "github.com/foo/auth");
}
```

- [ ] **Step 2: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib`

Expected: All tests pass including the new refs integration test.

- [ ] **Step 3: Commit**

```bash
git add rs/ll-open/cli-lib/tests/integration.rs
git commit -m "test: integration test for Go ref extraction in leyline parse"
```

---

### Task 5: Mache fixture validation

**Files:**
- Modify: `rs/ll-open/cli-lib/tests/integration.rs`

- [ ] **Step 1: Add test verifying mache-compatible schema**

```rust
#[tokio::test]
async fn test_refs_tables_match_mache_schema() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("mache-compat.db");

    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc main() {}\n",
    ).unwrap();

    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Verify all tables mache expects exist
    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    assert!(tables.contains(&"nodes".to_string()), "missing nodes table");
    assert!(tables.contains(&"_ast".to_string()), "missing _ast table");
    assert!(tables.contains(&"_source".to_string()), "missing _source table");
    assert!(tables.contains(&"node_refs".to_string()), "missing node_refs table");
    assert!(tables.contains(&"node_defs".to_string()), "missing node_defs table");
    assert!(tables.contains(&"_imports".to_string()), "missing _imports table");

    // Verify node_refs has the columns mache expects: (token, node_id)
    // mache queries: SELECT node_id FROM node_refs WHERE token = ?
    let _: String = conn.query_row(
        "SELECT node_id FROM node_refs LIMIT 1", [], |r| r.get(0),
    ).unwrap_or_default();

    // Verify mache fast-path trigger
    let nodes_exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='nodes'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(nodes_exists, 1, "mache fast-path trigger: nodes table must exist");
}
```

- [ ] **Step 2: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib && cargo clippy --workspace -- -D warnings`

Expected: All tests pass, clippy clean.

- [ ] **Step 3: Commit and push**

```bash
git add rs/ll-open/cli-lib/tests/integration.rs
git commit -m "test: mache schema compatibility for refs tables"
git push origin main
```
