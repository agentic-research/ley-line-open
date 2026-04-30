//! Go tree-sitter kind → CanonicalKind map.
//!
//! Same discipline as `rust.rs`: bucket each tree-sitter-go named kind into
//! one of seven canonical roles. Anything unmapped falls through to `Block`.
//! Don't churn entries without a basis bump.

use super::{CanonicalKind, CanonicalKindMap, FALLBACK_KIND};

pub struct GoCanonicalMap;

impl CanonicalKindMap for GoCanonicalMap {
    fn lookup(&self, kind: &str) -> CanonicalKind {
        match kind {
            // Declarations
            "function_declaration"
            | "method_declaration"
            | "type_declaration"
            | "type_spec"
            | "type_alias"
            | "var_declaration"
            | "var_spec"
            | "const_declaration"
            | "const_spec"
            | "import_declaration"
            | "import_spec"
            | "package_clause"
            | "parameter_declaration"
            | "field_declaration"
            | "struct_type"
            | "interface_type"
            | "method_spec"
            | "method_elem"
            | "type_parameter_declaration" => CanonicalKind::Decl,

            // Expressions
            "call_expression"
            | "selector_expression"
            | "index_expression"
            | "slice_expression"
            | "binary_expression"
            | "unary_expression"
            | "type_assertion_expression"
            | "type_conversion_expression"
            | "composite_literal"
            | "func_literal"
            | "parenthesized_expression"
            | "pointer_type"
            | "reference_expression" => CanonicalKind::Expr,

            // Statements
            "expression_statement"
            | "send_statement"
            | "go_statement"
            | "defer_statement"
            | "return_statement"
            | "break_statement"
            | "continue_statement"
            | "goto_statement"
            | "labeled_statement"
            | "fallthrough_statement"
            | "short_var_declaration"
            | "assignment_statement"
            | "inc_statement"
            | "dec_statement"
            | "if_statement"
            | "for_statement"
            | "switch_statement"
            | "type_switch_statement"
            | "select_statement" => CanonicalKind::Stmt,

            // Blocks / scopes
            "block"
            | "source_file"
            | "function_body"
            | "case_clause"
            | "default_case"
            | "communication_case"
            | "expression_case" => CanonicalKind::Block,

            // References
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "package_identifier"
            | "qualified_type"
            | "label_name" => CanonicalKind::Ref,

            // Literals
            "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "rune_literal"
            | "string_literal"
            | "raw_string_literal"
            | "interpreted_string_literal"
            | "true"
            | "false"
            | "nil"
            | "iota" => CanonicalKind::Lit,

            // Operators / control-flow tags
            "if" | "else" | "for" | "range" | "switch" | "case" | "default"
            | "break" | "continue" | "goto" | "return" | "go" | "defer"
            | "func" | "var" | "const" | "type" | "package" | "import" | "+"
            | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>" | "&&"
            | "||" | "!" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "="
            | ":=" | "+=" | "-=" | "*=" | "/=" | "<-" | "..." => {
                CanonicalKind::Op
            }

            _ => FALLBACK_KIND,
        }
    }

    fn lang(&self) -> &'static str {
        "go"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{
        assert_canonical_map_baseline, assert_kinds_cover_all_canonical, assert_kinds_match,
        RustCanonicalMap,
    };

    #[test]
    fn common_go_kinds_map_correctly() {
        assert_kinds_match(
            &GoCanonicalMap,
            &[
                ("function_declaration", CanonicalKind::Decl),
                ("call_expression", CanonicalKind::Expr),
                ("if_statement", CanonicalKind::Stmt),
                ("block", CanonicalKind::Block),
                ("identifier", CanonicalKind::Ref),
                ("int_literal", CanonicalKind::Lit),
                ("if", CanonicalKind::Op),
            ],
        );
    }

    #[test]
    fn go_map_covers_every_canonical_kind() {
        // GoCanonicalMap must produce at least one example for each
        // of the seven canonical roles. A refactor dropping an entire
        // bucket (e.g. removed every `CanonicalKind::Ref` arm) would
        // silently shift parsed Go output via FALLBACK_KIND. See the
        // shared helper for the diagnostic.
        assert_kinds_cover_all_canonical(
            &GoCanonicalMap,
            &[
                ("function_declaration", CanonicalKind::Decl),
                ("call_expression", CanonicalKind::Expr),
                ("if_statement", CanonicalKind::Stmt),
                ("block", CanonicalKind::Block),
                ("identifier", CanonicalKind::Ref),
                ("int_literal", CanonicalKind::Lit),
                ("if", CanonicalKind::Op),
            ],
        );
    }

    #[test]
    fn baseline_invariants() {
        // Shared invariants — see `canonical::assert_canonical_map_baseline`.
        assert_canonical_map_baseline(&GoCanonicalMap, "go");
    }

    #[test]
    fn cross_language_invariance_smoke_test() {
        // Hand-written `if-then-return` shape: Rust calls it
        // `if_expression`, Go calls it `if_statement`. Both must
        // produce the same canonical alphabet sequence at the
        // outer-shape level. This is the cross-language hotspot
        // detection invariance the canonical alphabet was designed
        // for. Not a full end-to-end test (no actual parsing), but
        // pins that `if`-equivalents collapse to a comparable
        // canonical role across the two languages.
        // Rust: if_expression (Expr) — Go: if_statement (Stmt)
        // These are NOT identical canonical roles because the
        // underlying language semantics differ (Rust's if is an
        // expression, Go's is a statement). But the *children* —
        // condition (Expr) and body (Block) — are the same. The
        // alphabet captures the universally-true facts, not
        // language-specific framing.
        assert_kinds_match(
            &RustCanonicalMap,
            &[
                ("identifier", CanonicalKind::Ref),
                ("integer_literal", CanonicalKind::Lit),
                ("block", CanonicalKind::Block),
            ],
        );
        assert_kinds_match(
            &GoCanonicalMap,
            &[
                ("identifier", CanonicalKind::Ref),
                ("int_literal", CanonicalKind::Lit),
                ("block", CanonicalKind::Block),
            ],
        );
    }
}
