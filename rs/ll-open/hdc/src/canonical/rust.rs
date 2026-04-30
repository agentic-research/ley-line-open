//! Rust tree-sitter kind → CanonicalKind map.
//!
//! Sourced from the tree-sitter-rust grammar's named node-kinds. Bucket each
//! kind into one of the seven canonical roles. Anything not listed falls
//! through to `FALLBACK_KIND` (Block).
//!
//! Adding to this map is non-breaking. *Removing* or *changing* an existing
//! entry is breaking — every hypervector encoded against the old mapping
//! becomes incomparable with new ones. Don't churn this without a basis bump.

use super::{CanonicalKind, CanonicalKindMap, FALLBACK_KIND};

pub struct RustCanonicalMap;

impl CanonicalKindMap for RustCanonicalMap {
    fn lookup(&self, kind: &str) -> CanonicalKind {
        match kind {
            // Declarations
            "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "trait_item"
            | "impl_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            | "use_declaration"
            | "extern_crate_declaration"
            | "let_declaration"
            | "macro_definition"
            | "parameter"
            | "self_parameter"
            | "field_declaration"
            | "enum_variant"
            | "associated_type"
            | "where_predicate" => CanonicalKind::Decl,

            // Expressions
            "call_expression"
            | "macro_invocation"
            | "field_expression"
            | "index_expression"
            | "method_call_expression"
            | "binary_expression"
            | "unary_expression"
            | "assignment_expression"
            | "compound_assignment_expr"
            | "type_cast_expression"
            | "reference_expression"
            | "closure_expression"
            | "range_expression"
            | "try_expression"
            | "await_expression"
            | "parenthesized_expression"
            | "tuple_expression"
            | "array_expression"
            | "struct_expression"
            | "if_expression"
            | "match_expression"
            | "while_expression"
            | "loop_expression"
            | "for_expression"
            | "yield_expression"
            | "async_block"
            | "unsafe_block" => CanonicalKind::Expr,

            // Statements
            "expression_statement"
            | "let_chain"
            | "return_expression"
            | "break_expression"
            | "continue_expression" => CanonicalKind::Stmt,

            // Blocks / scopes
            "block"
            | "source_file"
            | "declaration_list"
            | "field_declaration_list"
            | "enum_variant_list"
            | "match_block"
            | "match_arm" => CanonicalKind::Block,

            // References
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "scoped_use_list"
            | "use_list"
            | "shorthand_field_initializer"
            | "lifetime"
            | "self" => CanonicalKind::Ref,

            // Literals
            "integer_literal"
            | "float_literal"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "boolean_literal"
            | "byte_string_literal"
            | "raw_byte_string_literal" => CanonicalKind::Lit,

            // Operators / control-flow tags
            "if" | "else" | "match" | "while" | "loop" | "for" | "in" | "break"
            | "continue" | "return" | "yield" | "await" | "fn" | "let" | "const"
            | "static" | "mut" | "ref" | "&" | "*" | "+" | "-" | "/" | "%" | "&&"
            | "||" | "!" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "=" | "+="
            | "-=" | "*=" | "/=" | ".." | "..=" | "->" | "=>" | "::" | "?" => {
                CanonicalKind::Op
            }

            _ => FALLBACK_KIND,
        }
    }

    fn lang(&self) -> &'static str {
        "rust"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{assert_canonical_map_baseline, assert_kinds_match};

    #[test]
    fn common_rust_kinds_map_correctly() {
        assert_kinds_match(
            &RustCanonicalMap,
            &[
                ("function_item", CanonicalKind::Decl),
                ("if_expression", CanonicalKind::Expr),
                ("expression_statement", CanonicalKind::Stmt),
                ("block", CanonicalKind::Block),
                ("identifier", CanonicalKind::Ref),
                ("integer_literal", CanonicalKind::Lit),
                ("if", CanonicalKind::Op),
            ],
        );
    }

    #[test]
    fn baseline_invariants() {
        // Shared invariants every CanonicalKindMap must hold (forward-
        // compat fallback + lang identity). Defined in canonical.rs so
        // every future language gets them for free.
        assert_canonical_map_baseline(&RustCanonicalMap, "rust");
    }
}
