//! JSON tree-sitter kind → CanonicalKind map.
//!
//! Data languages don't have a 1:1 mapping to the seven canonical kinds the
//! way imperative languages do — JSON has no statements, no expressions, no
//! references. The map covers the four roles that *do* apply naturally:
//!
//!   - `pair`        → `Decl` (key:value declaration)
//!   - `object` / `array` / `document` → `Block` (containers)
//!   - scalars       → `Lit` (string, number, true, false, null, …)
//!   - `comment`     → `Op`  (syntactic markup, JSONC only)
//!
//! Anything else (including `_value` hidden productions, `escape_sequence`)
//! falls through to `FALLBACK_KIND` (`Block`). This is a deliberate partial
//! map — forcing every kind into a canonical role would inject noise into
//! HDC similarity scores between data files.

use super::{CanonicalKind, CanonicalKindMap, FALLBACK_KIND};

pub struct JsonCanonicalMap;

impl CanonicalKindMap for JsonCanonicalMap {
    fn lookup(&self, kind: &str) -> CanonicalKind {
        match kind {
            // Declarations: each pair declares a key→value binding.
            "pair" => CanonicalKind::Decl,

            // Containers: holds children, no semantic role of its own.
            "object" | "array" | "document" => CanonicalKind::Block,

            // Literals: terminal values.
            "string" | "string_content" | "number" | "true" | "false" | "null"
            | "escape_sequence" => CanonicalKind::Lit,

            // Operators / syntactic markup.
            "comment" => CanonicalKind::Op,

            // Unknown / unmapped → forward-compat fallback.
            _ => FALLBACK_KIND,
        }
    }

    fn lang(&self) -> &'static str {
        "json"
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
        assert_canonical_map_baseline(&JsonCanonicalMap, "json");
    }

    #[test]
    fn known_kinds_map_correctly() {
        // Pin every documented mapping. If a tree-sitter-json upgrade
        // renames a kind (e.g. `pair` → `member`), this test fails with
        // a clear message pointing at the drifted kind.
        assert_kinds_match(&JsonCanonicalMap, &[
            ("pair", CanonicalKind::Decl),
            ("object", CanonicalKind::Block),
            ("array", CanonicalKind::Block),
            ("document", CanonicalKind::Block),
            ("string", CanonicalKind::Lit),
            ("string_content", CanonicalKind::Lit),
            ("number", CanonicalKind::Lit),
            ("true", CanonicalKind::Lit),
            ("false", CanonicalKind::Lit),
            ("null", CanonicalKind::Lit),
            ("escape_sequence", CanonicalKind::Lit),
            ("comment", CanonicalKind::Op),
        ]);
    }

    #[test]
    fn json_pair_aligns_with_yaml_block_mapping_pair() {
        // Cross-grammar consistency: at registry-repo scale (helm/charts
        // mixes YAML + JSON), config files should produce comparable HDC
        // signatures regardless of serialization format. Pin that the
        // primary structural role (key:value declaration) maps the same
        // way under both grammars. If this drifts, JSON-vs-YAML cross-
        // similarity collapses silently.
        use crate::canonical::yaml::YamlCanonicalMap;
        assert_eq!(
            JsonCanonicalMap.lookup("pair"),
            YamlCanonicalMap.lookup("block_mapping_pair"),
            "JSON pair and YAML block_mapping_pair must share canonical role",
        );
        assert_eq!(
            JsonCanonicalMap.lookup("object"),
            YamlCanonicalMap.lookup("block_mapping"),
            "JSON object and YAML block_mapping must share canonical role",
        );
        assert_eq!(
            JsonCanonicalMap.lookup("array"),
            YamlCanonicalMap.lookup("block_sequence"),
            "JSON array and YAML block_sequence must share canonical role",
        );
    }
}
