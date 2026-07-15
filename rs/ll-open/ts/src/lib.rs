//! Tree-sitter AST projection into ley-line's `nodes` table.
//!
//! Parses HTML, Markdown, JSON, or YAML (or any tree-sitter grammar) and writes
//! the AST as a filesystem tree into a SQLite database compatible with leyline-fs.
//!
//! Standalone crate — depends on `rusqlite` + `tree-sitter` + `leyline-schema`.
//! Free of fuser/nfsserve/tokio dependencies.

pub mod cfg;
pub mod injections;
pub mod languages;
pub mod project;
#[cfg(feature = "pyproject")]
pub mod pyproject;
pub mod query_engine;
pub mod refs;
pub mod schema;
pub mod splice;

use anyhow::Result;
use rusqlite::Connection;

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
    let data = conn.serialize("main")?;
    Ok(data.to_vec())
}

/// Parse content and return serialized SQLite bytes (ready for arena load).
///
/// Backward-compatible: uses a default source ID. Prefer `parse_with_source`
/// for new code that needs splice support.
pub fn parse(content: &[u8], language: TsLanguage) -> Result<Vec<u8>> {
    let conn = Connection::open_in_memory()?;
    project_ast(content, language.ts_language(), &conn)?;
    let data = conn.serialize("main")?;
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

/// Convenience: parse HCL/Terraform content → serialized SQLite bytes.
/// Same grammar for `.hcl` / `.tf` / `.tfvars` — `tree-sitter-hcl` handles
/// all three. Pairs with `mache-d5e158` (consumer-side HCL ASTWalker parity).
#[cfg(feature = "hcl")]
pub fn parse_hcl(content: &[u8]) -> Result<Vec<u8>> {
    parse(content, TsLanguage::Hcl)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
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

    /// HCL/Terraform round-trip via the convenience `parse_hcl` (bead
    /// `ley-line-open-fa9ec3`). Pins:
    ///   1. `parse_hcl` is reachable and returns non-empty serialized bytes
    ///   2. `_ast` rows are emitted for a representative `.tf` source
    ///   3. HCL-grammar productions (`block`, `attribute`) appear in node_kind
    /// Together these prove mache-d5e158 can consume LLO's HCL `_ast` rows
    /// the same way it consumes Go rows today.
    #[cfg(feature = "hcl")]
    #[test]
    fn hcl_terraform_round_trip_emits_ast() {
        let src = br#"resource "aws_instance" "web" {
  ami           = "ami-12345"
  instance_type = "t3.micro"
}

variable "region" {
  default = "us-east-1"
}
"#;
        let bytes = parse_hcl(src).unwrap();
        assert!(
            !bytes.is_empty(),
            "parse_hcl should produce serialized bytes"
        );

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
            .unwrap();

        let ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(ast_count > 0, "hcl _ast should have rows");

        // HCL grammar emits `block` for `resource "..." "..." { ... }`
        // and `attribute` for the `key = value` rows. Pin both so a
        // future grammar bump that renames them surfaces here, not in
        // the consumer (mache).
        let block_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast WHERE node_kind = 'block'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            block_count >= 2,
            "expected ≥2 HCL blocks (resource + variable); got {block_count}"
        );

        let attr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast WHERE node_kind = 'attribute'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            attr_count >= 3,
            "expected ≥3 HCL attributes (ami, instance_type, default); got {attr_count}"
        );
    }

    /// HCL `.tfvars` (variable-assignment-only) round-trip via the
    /// generic `parse` entry point. Pins that the same grammar handles
    /// the three file shapes mache encounters in real Terraform repos.
    #[cfg(feature = "hcl")]
    #[test]
    fn hcl_tfvars_round_trip_emits_ast() {
        let src = b"region = \"us-west-2\"\ninstances = 3\n";
        let bytes = parse_with_source(src, TsLanguage::Hcl, "terraform.tfvars").unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
            .unwrap();

        let (src_id, lang): (String, String) = conn
            .query_row("SELECT id, language FROM _source LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(src_id, "terraform.tfvars");
        assert_eq!(lang, "hcl");

        let ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(ast_count > 0, ".tfvars _ast should have rows");
    }

    // ── Tier 1+2 grammar bulk (bead ley-line-open-46ae48) ──────────────
    //
    // Per-language golden-parse + broken-snippet fixtures for the 16
    // grammars registered at mache's CGO removal. Tier 1 contract: a
    // canonical snippet produces `_ast` rows with ZERO `ERROR` nodes.
    // Tier 2 contract: a broken snippet surfaces `ERROR` nodes in the
    // parse tree — the same nodes `leyline_fs::validate::
    // collect_syntax_errors` enumerates and the daemon `validate` op
    // serializes. ERROR nodes are named, so they land in `_ast` as
    // `node_kind = 'ERROR'` rows; asserting on `_ast` pins both the
    // grammar wiring and the projection in one pass.

    /// Golden parse: `src` must produce `_ast` rows and no `ERROR` rows.
    #[allow(dead_code)] // used only by feature-gated tests below
    fn assert_golden_parse(lang: TsLanguage, src: &[u8], source_id: &str) {
        let bytes = parse_with_source(src, lang, source_id).unwrap();
        assert!(!bytes.is_empty(), "{source_id}: serialized bytes empty");

        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
            .unwrap();

        let (src_id, db_lang): (String, String) = conn
            .query_row("SELECT id, language FROM _source LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(src_id, source_id);
        assert_eq!(db_lang, lang.name());

        let ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert!(ast_count > 0, "{source_id}: _ast should have rows");

        let error_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast WHERE node_kind = 'ERROR'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            error_count, 0,
            "{source_id}: canonical snippet must parse clean (no ERROR nodes)"
        );
    }

    /// Broken snippet: the parse must enumerate `ERROR` nodes — the
    /// substrate the validate op's ERROR/MISSING listing walks.
    #[allow(dead_code)] // used only by feature-gated tests below
    fn assert_broken_snippet_enumerates_errors(lang: TsLanguage, src: &[u8], source_id: &str) {
        let bytes = parse_with_source(src, lang, source_id).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        let cursor = Cursor::new(&bytes);
        conn.deserialize_read_exact("main", cursor, bytes.len(), true)
            .unwrap();

        let error_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast WHERE node_kind = 'ERROR'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            error_count > 0,
            "{source_id}: broken snippet must surface ERROR nodes in _ast"
        );
    }

    #[cfg(feature = "sql")]
    #[test]
    fn round_trip_sql() {
        assert_golden_parse(
            TsLanguage::Sql,
            b"CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n\
              SELECT id, name FROM users WHERE id = 1 ORDER BY name;\n\
              INSERT INTO users (id, name) VALUES (2, 'ada');\n",
            "schema.sql",
        );
    }

    #[cfg(feature = "sql")]
    #[test]
    fn sql_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Sql,
            b"SELECT FROM WHERE ((;\nINSERT users INTO;\n",
            "broken.sql",
        );
    }

    #[cfg(feature = "bash")]
    #[test]
    fn round_trip_bash() {
        assert_golden_parse(
            TsLanguage::Bash,
            b"#!/bin/sh\nset -eu\nfor f in *.txt; do\n  echo \"$f\"\ndone\n\
              greet() {\n  local name=\"$1\"\n  printf 'hi %s\\n' \"$name\"\n}\n",
            "script.sh",
        );
    }

    #[cfg(feature = "bash")]
    #[test]
    fn bash_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Bash,
            b"if [ -f x ]; then\ncase esac do done\n",
            "broken.sh",
        );
    }

    #[cfg(feature = "java")]
    #[test]
    fn round_trip_java() {
        assert_golden_parse(
            TsLanguage::Java,
            b"package example;\n\npublic class Greeter {\n\
              \tprivate final String name;\n\
              \tpublic Greeter(String name) { this.name = name; }\n\
              \tpublic String hello() { return \"Hello, \" + name; }\n}\n",
            "Greeter.java",
        );
    }

    #[cfg(feature = "java")]
    #[test]
    fn java_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Java,
            b"public class { void ??? (((\n",
            "Broken.java",
        );
    }

    #[cfg(feature = "c")]
    #[test]
    fn round_trip_c() {
        assert_golden_parse(
            TsLanguage::C,
            b"#include <stdio.h>\n\nint add(int a, int b) { return a + b; }\n\n\
              int main(void) {\n\tprintf(\"%d\\n\", add(1, 2));\n\treturn 0;\n}\n",
            "main.c",
        );
    }

    #[cfg(feature = "c")]
    #[test]
    fn c_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::C,
            b"int main( { return 0; }}}\n",
            "broken.c",
        );
    }

    #[cfg(feature = "cpp")]
    #[test]
    fn round_trip_cpp() {
        assert_golden_parse(
            TsLanguage::Cpp,
            b"#include <string>\n\nnamespace example {\n\
              class Greeter {\n public:\n\
              \texplicit Greeter(std::string name) : name_(std::move(name)) {}\n\
              \tstd::string hello() const { return \"Hello, \" + name_; }\n\
              private:\n\tstd::string name_;\n};\n}  // namespace example\n",
            "greeter.cpp",
        );
    }

    #[cfg(feature = "cpp")]
    #[test]
    fn cpp_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Cpp,
            b"class { void ??? (((\n",
            "broken.cpp",
        );
    }

    #[cfg(feature = "toml")]
    #[test]
    fn round_trip_toml() {
        assert_golden_parse(
            TsLanguage::Toml,
            b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
              [dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n",
            "Cargo.toml",
        );
    }

    #[cfg(feature = "toml")]
    #[test]
    fn toml_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Toml,
            b"key = = value\n[[[\n= no_key\n",
            "broken.toml",
        );
    }

    #[cfg(feature = "dockerfile")]
    #[test]
    fn round_trip_dockerfile() {
        assert_golden_parse(
            TsLanguage::Dockerfile,
            b"FROM alpine:3.20 AS build\nRUN apk add --no-cache curl\n\
              COPY . /app\nWORKDIR /app\nENV MODE=release\n\
              CMD [\"/app/run\"]\n",
            "app.dockerfile",
        );
    }

    #[cfg(feature = "dockerfile")]
    #[test]
    fn dockerfile_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Dockerfile,
            b"FROM\nCOPY\n<<<???\n",
            "broken.dockerfile",
        );
    }

    #[cfg(feature = "ruby")]
    #[test]
    fn round_trip_ruby() {
        assert_golden_parse(
            TsLanguage::Ruby,
            b"class Greeter\n  def initialize(name)\n    @name = name\n  end\n\n  \
              def hello\n    \"Hello, #{@name}!\"\n  end\nend\n",
            "greeter.rb",
        );
    }

    #[cfg(feature = "ruby")]
    #[test]
    fn ruby_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Ruby,
            b"def end (((\nclass 42\n",
            "broken.rb",
        );
    }

    #[cfg(feature = "php")]
    #[test]
    fn round_trip_php() {
        assert_golden_parse(
            TsLanguage::Php,
            b"<?php\n\nfunction greet(string $name): string {\n\
              \treturn \"Hello, {$name}!\";\n}\n\necho greet(\"ada\");\n",
            "greet.php",
        );
    }

    #[cfg(feature = "php")]
    #[test]
    fn php_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Php,
            b"<?php function ( { }\n",
            "broken.php",
        );
    }

    #[cfg(feature = "kotlin")]
    #[test]
    fn round_trip_kotlin() {
        assert_golden_parse(
            TsLanguage::Kotlin,
            b"package example\n\ndata class Greeter(val name: String) {\n\
              \tfun hello(): String = \"Hello, $name!\"\n}\n\n\
              fun main() {\n\tprintln(Greeter(\"ada\").hello())\n}\n",
            "Greeter.kt",
        );
    }

    #[cfg(feature = "kotlin")]
    #[test]
    fn kotlin_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Kotlin,
            b"fun ( { val = }\n",
            "Broken.kt",
        );
    }

    #[cfg(feature = "swift")]
    #[test]
    fn round_trip_swift() {
        assert_golden_parse(
            TsLanguage::Swift,
            b"struct Greeter {\n\tlet name: String\n\n\
              \tfunc hello() -> String {\n\t\treturn \"Hello, \\(name)!\"\n\t}\n}\n\n\
              let g = Greeter(name: \"ada\")\nprint(g.hello())\n",
            "greeter.swift",
        );
    }

    #[cfg(feature = "swift")]
    #[test]
    fn swift_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Swift,
            b"func { let = ) }\n",
            "broken.swift",
        );
    }

    #[cfg(feature = "scala")]
    #[test]
    fn round_trip_scala() {
        assert_golden_parse(
            TsLanguage::Scala,
            b"package example\n\ncase class Greeter(name: String) {\n\
              \tdef hello: String = s\"Hello, $name!\"\n}\n\n\
              object Main extends App {\n\tprintln(Greeter(\"ada\").hello)\n}\n",
            "Greeter.scala",
        );
    }

    #[cfg(feature = "scala")]
    #[test]
    fn scala_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Scala,
            b"class { def = )\n",
            "Broken.scala",
        );
    }

    #[cfg(feature = "csharp")]
    #[test]
    fn round_trip_csharp() {
        assert_golden_parse(
            TsLanguage::CSharp,
            b"namespace Example;\n\npublic class Greeter\n{\n\
              \tprivate readonly string _name;\n\
              \tpublic Greeter(string name) => _name = name;\n\
              \tpublic string Hello() => $\"Hello, {_name}!\";\n}\n",
            "Greeter.cs",
        );
    }

    #[cfg(feature = "csharp")]
    #[test]
    fn csharp_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::CSharp,
            b"public class { void ??? (((\n",
            "Broken.cs",
        );
    }

    #[cfg(feature = "css")]
    #[test]
    fn round_trip_css() {
        assert_golden_parse(
            TsLanguage::Css,
            b".greeter {\n\tcolor: #333;\n\tmargin: 0 auto;\n}\n\n\
              @media (max-width: 600px) {\n\t.greeter { display: none; }\n}\n",
            "style.css",
        );
    }

    #[cfg(feature = "css")]
    #[test]
    fn css_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Css,
            b"a { color: ; } } }\n@media {{{\n",
            "broken.css",
        );
    }

    #[cfg(feature = "groovy")]
    #[test]
    fn round_trip_groovy() {
        assert_golden_parse(
            TsLanguage::Groovy,
            // Plain constructor call: murtaza64/tree-sitter-groovy 0.1
            // does not parse Groovy's named-argument constructor sugar
            // (`new Greeter(name: 'ada')` yields an ERROR node).
            b"class Greeter {\n\tString name\n\n\
              \tString hello() {\n\t\treturn \"Hello, ${name}!\"\n\t}\n}\n\n\
              def g = new Greeter()\nprintln(g.hello())\n",
            "Greeter.groovy",
        );
    }

    #[cfg(feature = "groovy")]
    #[test]
    fn groovy_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Groovy,
            b"def ( {{{ )))\n",
            "Broken.groovy",
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn round_trip_lua() {
        assert_golden_parse(
            TsLanguage::Lua,
            b"local Greeter = {}\nGreeter.__index = Greeter\n\n\
              function Greeter.new(name)\n\tlocal self = setmetatable({}, Greeter)\n\
              \tself.name = name\n\treturn self\nend\n\n\
              function Greeter:hello()\n\treturn \"Hello, \" .. self.name\nend\n",
            "greeter.lua",
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn lua_broken_snippet_enumerates_errors() {
        assert_broken_snippet_enumerates_errors(
            TsLanguage::Lua,
            b"function end (((\n",
            "broken.lua",
        );
    }
}
