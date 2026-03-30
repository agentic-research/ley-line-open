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
    let mut parser = Parser::new();
    parser.set_language(language).map_err(|e| ValidationError {
        line: 0,
        column: 0,
        message: format!("language init: {e}"),
    })?;

    let tree = parser.parse(content, None).ok_or(ValidationError {
        line: 0,
        column: 0,
        message: "tree-sitter parse returned None".into(),
    })?;

    let root = tree.root_node();
    if !root.has_error() {
        return Ok(());
    }

    // DFS for first ERROR/MISSING node (same as mache's findFirstError)
    match find_first_error(root) {
        Some(node) => Err(ValidationError {
            line: node.start_position().row as u32,
            column: node.start_position().column as u32,
            message: "syntax error".into(),
        }),
        None => Err(ValidationError {
            line: 0,
            column: 0,
            message: "AST contains errors".into(),
        }),
    }
}

#[cfg(feature = "validate")]
fn find_first_error(node: TsNode) -> Option<TsNode> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if (child.has_error() || child.is_error() || child.is_missing())
            && let Some(found) = find_first_error(child)
        {
            return Some(found);
        }
    }
    None
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
