//! YAML tree-sitter kind → CanonicalKind map.
//!
//! YAML maps to a richer subset of the canonical alphabet than JSON because
//! the format adds anchors/aliases (Decl/Ref pair), tags (Op), and directives
//! (Decl). The mapping aligns YAML's structural roles with JSON's so a
//! mixed-config repo (helm/charts: YAML + JSON together) produces comparable
//! HDC signatures regardless of serialization format.
//!
//! Roles assigned:
//!   - Declarations: `block_mapping_pair`, `flow_pair`, `anchor`, directives
//!   - References:   `alias` (anchor-back-reference)
//!   - Operators:    `tag`, `comment`
//!   - Literals:     all `*_scalar` variants and `escape_sequence`
//!   - Containers:   `document`, `stream`, `block_*` / `flow_*` collections
//!     and node wrappers (`block_node`, `flow_node`)
//!
//! Anonymous productions (`alias_name`, `anchor_name`, `directive_name`,
//! `tag_handle`, `tag_prefix`, `yaml_version`, `directive_parameter`) are
//! identifier-fragment carriers and fall through to `FALLBACK_KIND` —
//! mapping them as `Ref` would inflate the Ref-class population without
//! adding semantic signal.

use super::{CanonicalKind, CanonicalKindMap, FALLBACK_KIND};

pub struct YamlCanonicalMap;

impl CanonicalKindMap for YamlCanonicalMap {
    fn lookup(&self, kind: &str) -> CanonicalKind {
        match kind {
            // Declarations: bind a name (or sequence position) to a value.
            "block_mapping_pair"
            | "flow_pair"
            | "anchor"
            | "yaml_directive"
            | "tag_directive"
            | "reserved_directive" => CanonicalKind::Decl,

            // References: alias resolves back to a previously-declared anchor.
            "alias" => CanonicalKind::Ref,

            // Statements: each sequence item is a positional declaration of
            // an item-in-a-list; treat as Stmt to mirror Go's list-element
            // statement role.
            "block_sequence_item" => CanonicalKind::Stmt,

            // Operators / syntactic markup. Tags annotate types; comments
            // are non-semantic but structurally present.
            "tag" | "comment" => CanonicalKind::Op,

            // Literals: every scalar form.
            "plain_scalar"
            | "string_scalar"
            | "single_quote_scalar"
            | "double_quote_scalar"
            | "block_scalar"
            | "integer_scalar"
            | "float_scalar"
            | "boolean_scalar"
            | "null_scalar"
            | "timestamp_scalar"
            | "escape_sequence" => CanonicalKind::Lit,

            // Containers.
            "document"
            | "stream"
            | "block_node"
            | "flow_node"
            | "block_mapping"
            | "flow_mapping"
            | "block_sequence"
            | "flow_sequence" => CanonicalKind::Block,

            // Unknown / unmapped → forward-compat fallback.
            _ => FALLBACK_KIND,
        }
    }

    fn lang(&self) -> &'static str {
        "yaml"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{
        assert_canonical_map_baseline, assert_kinds_match, CanonicalKind,
    };

    #[test]
    fn baseline_invariants() {
        assert_canonical_map_baseline(&YamlCanonicalMap, "yaml");
    }

    #[test]
    fn known_kinds_map_correctly() {
        assert_kinds_match(&YamlCanonicalMap, &[
            // Decl family.
            ("block_mapping_pair", CanonicalKind::Decl),
            ("flow_pair", CanonicalKind::Decl),
            ("anchor", CanonicalKind::Decl),
            ("yaml_directive", CanonicalKind::Decl),
            ("tag_directive", CanonicalKind::Decl),
            ("reserved_directive", CanonicalKind::Decl),

            // Ref + Stmt.
            ("alias", CanonicalKind::Ref),
            ("block_sequence_item", CanonicalKind::Stmt),

            // Op family.
            ("tag", CanonicalKind::Op),
            ("comment", CanonicalKind::Op),

            // Lit family — every scalar shape.
            ("plain_scalar", CanonicalKind::Lit),
            ("string_scalar", CanonicalKind::Lit),
            ("single_quote_scalar", CanonicalKind::Lit),
            ("double_quote_scalar", CanonicalKind::Lit),
            ("block_scalar", CanonicalKind::Lit),
            ("integer_scalar", CanonicalKind::Lit),
            ("float_scalar", CanonicalKind::Lit),
            ("boolean_scalar", CanonicalKind::Lit),
            ("null_scalar", CanonicalKind::Lit),
            ("timestamp_scalar", CanonicalKind::Lit),
            ("escape_sequence", CanonicalKind::Lit),

            // Block family — containers.
            ("document", CanonicalKind::Block),
            ("stream", CanonicalKind::Block),
            ("block_node", CanonicalKind::Block),
            ("flow_node", CanonicalKind::Block),
            ("block_mapping", CanonicalKind::Block),
            ("flow_mapping", CanonicalKind::Block),
            ("block_sequence", CanonicalKind::Block),
            ("flow_sequence", CanonicalKind::Block),
        ]);
    }

    #[test]
    fn anchor_alias_pair_uses_decl_ref_roles() {
        // Cross-grammar consistency for HDC encoding: a YAML anchor
        // declares a name and an alias references that name — same
        // declarative-then-referential pattern as Go's
        // `var x = ...` + `x` later. Pin so a refactor that downgraded
        // alias to Block would silently lose anchor-tracking signal.
        assert_eq!(YamlCanonicalMap.lookup("anchor"), CanonicalKind::Decl);
        assert_eq!(YamlCanonicalMap.lookup("alias"), CanonicalKind::Ref);
    }

    #[test]
    fn anonymous_productions_fall_through_to_block() {
        // Fragment-carrier nodes (anchor_name, alias_name, tag_handle,
        // directive_name, directive_parameter, yaml_version, tag_prefix)
        // are documented as identifier carriers — they're parsed as
        // named children but contribute no structural role. Pin that
        // the map deliberately leaves them unclassified so the test
        // doesn't accidentally start counting them under Ref/Decl
        // populations.
        for fragment in [
            "anchor_name",
            "alias_name",
            "tag_handle",
            "directive_name",
            "directive_parameter",
            "yaml_version",
            "tag_prefix",
        ] {
            assert_eq!(
                YamlCanonicalMap.lookup(fragment),
                FALLBACK_KIND,
                "{fragment} must fall through to FALLBACK_KIND",
            );
        }
    }
}
