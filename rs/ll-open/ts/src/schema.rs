//! Schema definitions for the AST projection tables.
//!
//! Re-exports the shared `nodes` table from `leyline-schema` and adds
//! AST-specific tables (`_source`, `_ast`) that enable bidirectional splicing.

pub use leyline_schema::{NODES_DDL, create_schema, insert_node};

use anyhow::Result;
use rusqlite::{Connection, params};

/// DDL for the `_source` table — stores original source text for splice reconstruction.
pub const SOURCE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _source (
    id TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    content BLOB NOT NULL
);";

/// DDL for the `_ast` table — maps node IDs to byte ranges in the source.
pub const AST_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast (
    node_id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    node_kind TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_row INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_row INTEGER NOT NULL,
    end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ast_source ON _ast(source_id);";

/// Create `nodes`, `_source`, and `_ast` tables (idempotent).
pub fn create_ast_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
    conn.execute_batch(AST_DDL)?;
    Ok(())
}

/// Insert or replace a source row.
pub fn insert_source(conn: &Connection, id: &str, language: &str, content: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _source (id, language, content) VALUES (?1, ?2, ?3)",
        params![id, language, content],
    )?;
    Ok(())
}

/// Insert an AST byte-range mapping.
#[allow(clippy::too_many_arguments)]
pub fn insert_ast(
    conn: &Connection,
    node_id: &str,
    source_id: &str,
    node_kind: &str,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> Result<()> {
    conn.execute(
        "INSERT INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, \
         start_row, start_col, end_row, end_col) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row,
            end_col
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Refs / Defs / Imports tables
// ---------------------------------------------------------------------------

/// DDL for the `node_refs` table — stores identifier references.
pub const REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);";

/// DDL for the `node_defs` table — stores identifier definitions.
pub const DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);";

/// DDL for the `_imports` table — stores import/require mappings.
pub const IMPORTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);";

/// Create `node_refs`, `node_defs`, and `_imports` tables (idempotent).
pub fn create_refs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_DDL)?;
    conn.execute_batch(DEFS_DDL)?;
    conn.execute_batch(IMPORTS_DDL)?;
    Ok(())
}

/// Insert a reference row.
pub fn insert_ref(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_refs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert a definition row.
pub fn insert_def(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_defs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert an import row.
pub fn insert_import(conn: &Connection, alias: &str, path: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO _imports (alias, path, source_id) VALUES (?1, ?2, ?3)",
        params![alias, path, source_id],
    )?;
    Ok(())
}

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
