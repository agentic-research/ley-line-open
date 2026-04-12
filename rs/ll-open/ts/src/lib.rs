//! Tree-sitter AST projection into ley-line's `nodes` table.
//!
//! Parses HTML, Markdown, JSON, or YAML (or any tree-sitter grammar) and writes
//! the AST as a filesystem tree into a SQLite database compatible with leyline-fs.
//!
//! Standalone crate — depends on `rusqlite` + `tree-sitter` + `leyline-schema`.
//! Free of fuser/nfsserve/tokio dependencies.

pub mod languages;
pub mod project;
#[cfg(feature = "pyproject")]
pub mod pyproject;
pub mod refs;
pub mod schema;
pub mod splice;

use anyhow::Result;
use rusqlite::{Connection, DatabaseName};

use crate::languages::TsLanguage;
use crate::project::{project_ast, project_ast_with_source};

/// Parse content with source tracking and return serialized SQLite bytes.
///
/// Stores the original source in `_source`, byte ranges in `_ast`, and
/// plain-text records in `nodes` — ready for bidirectional splicing.
pub fn parse_with_source(content: &[u8], language: TsLanguage, source_id: &str) -> Result<Vec<u8>> {
    let conn = Connection::open_in_memory()?;
    let language_name = language.name();
    project_ast_with_source(
        content,
        language.ts_language(),
        &conn,
        source_id,
        language_name,
    )?;
    let data = conn.serialize(DatabaseName::Main)?;
    Ok(data.to_vec())
}

/// Parse content and return serialized SQLite bytes (ready for arena load).
///
/// Backward-compatible: uses a default source ID. Prefer `parse_with_source`
/// for new code that needs splice support.
pub fn parse(content: &[u8], language: TsLanguage) -> Result<Vec<u8>> {
    let conn = Connection::open_in_memory()?;
    project_ast(content, language.ts_language(), &conn)?;
    let data = conn.serialize(DatabaseName::Main)?;
    Ok(data.to_vec())
}

/// Convenience: parse HTML content → serialized SQLite bytes.
#[cfg(feature = "html")]
pub fn parse_html(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Html)
}

/// Convenience: parse Markdown content → serialized SQLite bytes.
#[cfg(feature = "markdown")]
pub fn parse_markdown(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Markdown)
}

/// Convenience: parse JSON content → serialized SQLite bytes.
#[cfg(feature = "json")]
pub fn parse_json(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Json)
}

/// Convenience: parse YAML content → serialized SQLite bytes.
#[cfg(feature = "yaml")]
pub fn parse_yaml(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Yaml)
}

/// Convenience: parse Go content → serialized SQLite bytes.
#[cfg(feature = "go")]
pub fn parse_go(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Go)
}

/// Convenience: parse Python content → serialized SQLite bytes.
#[cfg(feature = "python")]
pub fn parse_python(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Python)
}

/// Convenience: parse Elixir content → serialized SQLite bytes.
#[cfg(feature = "elixir")]
pub fn parse_elixir(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Elixir)
}

/// Lower-level: write AST nodes into an existing rusqlite Connection.
pub fn project_into(content: &[u8], language: TsLanguage, conn: &Connection) -> Result<()> {
    project_ast(content, language.ts_language(), conn)
}

/// Lower-level: write AST nodes with source tracking into an existing Connection.
pub fn project_into_with_source(
    content: &[u8],
    language: TsLanguage,
    conn: &Connection,
    source_id: &str,
) -> Result<()> {
    let language_name = language.name();
    project_ast_with_source(
        content,
        language.ts_language(),
        conn,
        source_id,
        language_name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[cfg(feature = "html")]
    #[test]
    fn round_trip_db() {
        let bytes = parse_html(b"<p>Hello</p>").unwrap();
        assert!(!bytes.is_empty());

        // Verify the bytes are loadable via sqlite3_deserialize
        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "round-trip db should have nodes");
    }

    #[cfg(feature = "html")]
    #[test]
    fn schema_matches_contract() {
        let bytes = parse_html(b"<div>x</div>").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        // Verify all expected columns exist by querying them
        let row: (String, String, String, i32, i64, i64, String) = conn
            .query_row(
                "SELECT id, parent_id, name, kind, size, mtime, record FROM nodes LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();

        // Root node
        assert_eq!(row.0, ""); // id
        assert_eq!(row.3, 1); // kind = dir
    }

    #[cfg(feature = "html")]
    #[test]
    fn parse_with_source_has_tables() {
        let bytes = parse_with_source(b"<p>Hi</p>", TsLanguage::Html, "test.html").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        // _source table populated
        let (src_id, lang): (String, String) = conn
            .query_row("SELECT id, language FROM _source LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(src_id, "test.html");
        assert_eq!(lang, "html");

        // _ast table populated
        let ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(ast_count > 0, "_ast should have rows");
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn round_trip_markdown() {
        let bytes = parse_markdown(b"# Hello\n\nWorld\n").unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "markdown round-trip should have nodes");
    }

    #[cfg(feature = "json")]
    #[test]
    fn round_trip_json() {
        let bytes = parse_json(br#"{"key": "value", "list": [1, 2, 3]}"#).unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "json round-trip should have nodes");
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn round_trip_yaml() {
        let bytes = parse_yaml(b"key: value\nlist:\n  - one\n  - two\n").unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "yaml round-trip should have nodes");
    }

    #[cfg(feature = "go")]
    #[test]
    fn round_trip_go() {
        let bytes = parse_go(b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n").unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "go round-trip should have nodes");
    }

    #[cfg(feature = "python")]
    #[test]
    fn round_trip_python() {
        let src = br#"
import argparse
import torch

def load_model(model_path="./model"):
    """Load the trained model."""
    if not model_path:
        raise FileNotFoundError("No model found")
    return model_path

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("input", nargs="?")
    args = parser.parse_args()
    if args.input:
        result = load_model(args.input)
        print(result)

if __name__ == "__main__":
    main()
"#;
        let bytes = parse_python(src).unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "python round-trip should have nodes");
    }

    #[cfg(feature = "elixir")]
    #[test]
    fn round_trip_elixir() {
        let src = br#"
defmodule Greeter do
  def hello(name) do
    "Hello, #{name}!"
  end
end
"#;
        let bytes = parse_elixir(src).unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "elixir round-trip should have nodes");
    }

    #[cfg(feature = "python")]
    #[test]
    fn python_with_source_tracking() {
        let src = b"def greet(name):\n    return f\"Hello, {name}\"\n";
        let bytes = parse_with_source(src, TsLanguage::Python, "greet.py").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, bytes.len(), true)
            .unwrap();

        let (src_id, lang): (String, String) = conn
            .query_row("SELECT id, language FROM _source LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(src_id, "greet.py");
        assert_eq!(lang, "python");

        let ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(ast_count > 0, "python _ast should have rows");
    }
}
