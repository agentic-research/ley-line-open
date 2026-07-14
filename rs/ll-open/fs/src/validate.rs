//! Tree-sitter validation for source files.
//!
//! Mirrors mache's `writeback/validate.go` — same languages, same error semantics.
//! Gated behind the `validate` feature flag.

#[cfg(feature = "validate")]
use tree_sitter::{Language, Node as TsNode, Parser};

/// Structured validation error with source location.
#[cfg(feature = "validate")]
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub line: u32,
    pub column: u32,
    pub message: String,
}

#[cfg(feature = "validate")]
impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line + 1, self.column + 1, self.message)
    }
}

/// One ERROR or MISSING node from a tree-sitter parse, with both
/// row/col and byte-range positions (bead ley-line-open-736800).
///
/// Consumed by the daemon's `validate` op, which serializes the full
/// list so callers (mache's write-back draft mode) can render every
/// syntax error into `_diagnostics/ast-errors` — not just the first.
#[cfg(feature = "validate")]
#[derive(Debug, Clone)]
pub struct SyntaxError {
    /// 0-based row of the node's start position.
    pub row: u32,
    /// 0-based column (byte offset within the row) of the node's start.
    pub col: u32,
    /// Byte offset of the node's start in the source buffer.
    pub byte_start: usize,
    /// Byte offset one past the node's end in the source buffer.
    /// For MISSING nodes this equals `byte_start` (zero-width).
    pub byte_end: usize,
    /// `"syntax error"` for ERROR nodes; `"missing <kind>"` for
    /// MISSING nodes (the kind is the token tree-sitter expected).
    pub message: String,
}

/// Map file extension to tree-sitter language.
/// Mirrors mache's `LanguageForPath()` — same extensions, same languages.
#[cfg(feature = "validate")]
pub fn language_for_extension(ext: &str) -> Option<Language> {
    match ext {
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "tsx" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "ex" | "exs" => Some(tree_sitter_elixir::LANGUAGE.into()),
        _ => None,
    }
}

/// Determine language from a node ID path.
///
/// Checks the last path component for a file extension first (e.g. `main.go` → Go).
/// If the file has an extension but it's not a recognized language, returns `None`
/// (the file is not source code — don't validate it).
/// Only falls back to the default language for truly extensionless files (like `source`).
#[cfg(feature = "validate")]
pub fn language_for_node(node_id: &str, fallback: Option<&Language>) -> Option<Language> {
    let name = node_id.rsplit('/').next().unwrap_or(node_id);
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot + 1..];
        // Has an extension — use it if recognized, otherwise skip validation entirely
        return language_for_extension(ext);
    }
    // No extension (e.g. "source") — use fallback language
    fallback.cloned()
}

/// Validate content against a tree-sitter grammar.
///
/// Returns `Ok(())` if the AST has no errors.
/// Returns `Err(ValidationError)` with line:col of the first syntax error.
#[cfg(feature = "validate")]
pub fn validate(content: &[u8], language: &Language) -> Result<(), ValidationError> {
    let errors = collect_syntax_errors(content, language).map_err(|e| ValidationError {
        line: 0,
        column: 0,
        message: e,
    })?;
    match errors.first() {
        None => Ok(()),
        Some(first) => Err(ValidationError {
            line: first.row,
            column: first.col,
            // Fixed message preserves the pre-736800 write-back contract
            // (callers pin on the literal "syntax error" / "AST contains
            // errors" strings); the per-node message lives on SyntaxError.
            message: if first.message == "AST contains errors" {
                first.message.clone()
            } else {
                "syntax error".into()
            },
        }),
    }
}

/// Parse `content` and return EVERY ERROR/MISSING node, in document
/// (pre-order DFS) order (bead ley-line-open-736800).
///
/// - `Ok(vec![])` — the parse is clean.
/// - `Ok(errors)` — one entry per ERROR or MISSING node.
/// - `Err(message)` — the parser itself failed (grammar init / parse
///   returned `None`); there is no tree to report positions from.
///
/// Uses the same tree-sitter grammars the `_ast` producer uses, so a
/// buffer that validates here parses identically at ingest time.
#[cfg(feature = "validate")]
pub fn collect_syntax_errors(
    content: &[u8],
    language: &Language,
) -> Result<Vec<SyntaxError>, String> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|e| format!("language init: {e}"))?;

    let tree = parser
        .parse(content, None)
        .ok_or_else(|| "tree-sitter parse returned None".to_string())?;

    let root = tree.root_node();
    if !root.has_error() {
        return Ok(Vec::new());
    }

    let mut errors = Vec::new();
    collect_errors_dfs(root, &mut errors);
    if errors.is_empty() {
        // Defensive: root.has_error() with no reachable ERROR/MISSING
        // node. Not observed in practice; surface a positionless entry
        // rather than a false ok.
        errors.push(SyntaxError {
            row: 0,
            col: 0,
            byte_start: 0,
            byte_end: 0,
            message: "AST contains errors".into(),
        });
    }
    Ok(errors)
}

#[cfg(feature = "validate")]
fn collect_errors_dfs(node: TsNode, out: &mut Vec<SyntaxError>) {
    if node.is_error() || node.is_missing() {
        out.push(SyntaxError {
            row: node.start_position().row as u32,
            col: node.start_position().column as u32,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            message: if node.is_missing() {
                format!("missing {}", node.kind())
            } else {
                "syntax error".into()
            },
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Prune subtrees with no errors anywhere; has_error() is true
        // for ERROR/MISSING nodes themselves and any ancestor of one.
        if child.has_error() || child.is_error() || child.is_missing() {
            collect_errors_dfs(child, out);
        }
    }
}

#[cfg(all(test, feature = "validate"))]
mod tests {
    use super::*;

    #[test]
    fn validate_valid_go() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n";
        assert!(validate(src, &lang).is_ok());
    }

    #[test]
    fn validate_invalid_go() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc {{{ bad\n";
        let err = validate(src, &lang).unwrap_err();
        // Error should be on line 2 (0-indexed), somewhere after "func"
        assert!(
            err.line >= 2,
            "expected error on line >= 2, got {}",
            err.line
        );
        assert_eq!(err.message, "syntax error");
    }

    #[test]
    fn validate_valid_rust() {
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
        let src = b"fn main() {\n    println!(\"hello\");\n}\n";
        assert!(validate(src, &lang).is_ok());
    }

    #[test]
    fn validate_invalid_rust() {
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
        let src = b"fn main( {\n";
        let err = validate(src, &lang).unwrap_err();
        assert_eq!(err.message, "syntax error");
    }

    #[test]
    fn language_for_extension_maps_correctly() {
        assert!(language_for_extension("go").is_some());
        assert!(language_for_extension("py").is_some());
        assert!(language_for_extension("js").is_some());
        assert!(language_for_extension("ts").is_some());
        assert!(language_for_extension("tsx").is_some());
        assert!(language_for_extension("rs").is_some());
        assert!(language_for_extension("txt").is_none());
        assert!(language_for_extension("").is_none());
    }

    #[test]
    fn language_for_node_with_extension() {
        // File with .go extension — should detect Go regardless of fallback
        let lang = language_for_node("functions/main.go", None);
        assert!(lang.is_some());

        // Nested path
        let lang = language_for_node("src/lib/utils.py", None);
        assert!(lang.is_some());
    }

    #[test]
    fn language_for_node_fallback() {
        let go_lang: Language = tree_sitter_go::LANGUAGE.into();

        // Extensionless "source" file — uses fallback
        let lang = language_for_node("functions/HandleRequest/source", Some(&go_lang));
        assert!(lang.is_some());

        // Extensionless with no fallback — returns None
        let lang = language_for_node("functions/HandleRequest/source", None);
        assert!(lang.is_none());
    }

    // ── collect_syntax_errors (bead ley-line-open-736800) ──────────

    #[test]
    fn collect_valid_go_returns_empty() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n";
        let errors = collect_syntax_errors(src, &lang).unwrap();
        assert!(
            errors.is_empty(),
            "clean parse must yield no errors: {errors:?}"
        );
    }

    #[test]
    fn collect_invalid_go_positions_within_buffer() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc {{{ bad\n";
        let errors = collect_syntax_errors(src, &lang).unwrap();
        assert!(!errors.is_empty(), "broken Go must yield errors");
        for e in &errors {
            assert!(
                e.byte_start <= e.byte_end && e.byte_end <= src.len(),
                "byte range out of bounds: {e:?} (len {})",
                src.len()
            );
            assert!(!e.message.is_empty(), "message must be non-empty: {e:?}");
        }
    }

    #[test]
    fn collect_first_error_agrees_with_validate() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc {{{ bad\n";
        let errors = collect_syntax_errors(src, &lang).unwrap();
        let first = errors.first().expect("broken Go must yield errors");
        let legacy = validate(src, &lang).unwrap_err();
        assert_eq!(
            first.row, legacy.line,
            "first error row must match validate()"
        );
        assert_eq!(
            first.col, legacy.column,
            "first error col must match validate()"
        );
    }

    #[test]
    fn collect_missing_node_reports_expected_token() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        // Unclosed brace — tree-sitter recovers with a zero-width
        // MISSING "}" node at the end of the buffer.
        let src = b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n";
        let errors = collect_syntax_errors(src, &lang).unwrap();
        assert!(!errors.is_empty(), "unclosed brace must yield errors");
        let missing: Vec<_> = errors
            .iter()
            .filter(|e| e.message.starts_with("missing "))
            .collect();
        assert!(
            !missing.is_empty(),
            "expected a MISSING node with `missing <kind>` message; got {errors:?}"
        );
        for m in missing {
            assert_eq!(
                m.byte_start, m.byte_end,
                "MISSING nodes are zero-width: {m:?}"
            );
        }
    }

    #[test]
    fn collect_multiple_errors_in_document_order() {
        let lang: Language = tree_sitter_go::LANGUAGE.into();
        let src = b"package main\n\nfunc a( {\n}\n\nfunc b( {\n}\n";
        let errors = collect_syntax_errors(src, &lang).unwrap();
        assert!(
            errors.len() >= 2,
            "two broken funcs must yield >= 2 errors; got {errors:?}"
        );
        assert!(
            errors
                .windows(2)
                .all(|w| w[0].byte_start <= w[1].byte_start),
            "errors must be in document order: {errors:?}"
        );
    }

    #[test]
    fn language_for_node_unknown_extension() {
        let go_lang: Language = tree_sitter_go::LANGUAGE.into();

        // Unknown extension — returns None even with fallback (has extension = not extensionless)
        let lang = language_for_node("data/config.toml", Some(&go_lang));
        assert!(lang.is_none(), ".toml should not use fallback");

        // Unknown extension without fallback — also None
        let lang = language_for_node("data/config.toml", None);
        assert!(lang.is_none());
    }
}
