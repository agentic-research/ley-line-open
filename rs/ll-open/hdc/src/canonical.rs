//! Canonical-kind alphabet — the highest-leverage decision in the HDC stack.
//!
//! Math-friend review (bead `ley-line-open-96b1a9`) flagged this as the single
//! decision that determines whether cross-grammar / cross-version structural
//! similarity works at all. Raw parser kind names aren't grammar-stable:
//! tree-sitter renames `if_statement` → `if_clause` between versions; ANTLR
//! and tree-sitter use different production granularity. Mapping every
//! parser-kind into a 7-element canonical alphabet collapses that noise.
//!
//! Two functions with the same control-flow shape, written in Rust vs Go
//! vs Python, produce the same canonical-kind sequence even when their
//! parser-given kind-names diverge.
//!
//! Alphabet derives from Deckard (Jiang et al. 2007) and similar
//! clone-detection literature — converged on a small (~7-element)
//! canonical alphabet across multiple lines of work.

/// The seven structural roles every parser node maps into. Fits in 3 bits;
/// chosen to cover the AST primitives every imperative language exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonicalKind {
    /// Declarations — `fn`, `class`, `struct`, `let`, `var`, `type`, etc.
    Decl,
    /// Expressions — calls, binary ops, member access, indexing, lambdas.
    Expr,
    /// Statements — assignments, returns, throws, expression-statements.
    Stmt,
    /// Compound containers — blocks, scopes, modules, function bodies.
    Block,
    /// Identifier/path references — `foo`, `std::collections::HashMap`.
    Ref,
    /// Literals — numbers, strings, bools, nil.
    Lit,
    /// Operators + control-flow tags — `+`, `-`, `if`, `while`, `match`.
    Op,
}

impl CanonicalKind {
    /// 3-bit discriminant — used for hashing the canonical sequence into
    /// hypervector seeds. Stable across versions; never change once
    /// committed (the hash output depends on these values).
    pub fn discriminant(&self) -> u8 {
        match self {
            CanonicalKind::Decl => 0,
            CanonicalKind::Expr => 1,
            CanonicalKind::Stmt => 2,
            CanonicalKind::Block => 3,
            CanonicalKind::Ref => 4,
            CanonicalKind::Lit => 5,
            CanonicalKind::Op => 6,
        }
    }

    /// All seven canonical kinds in discriminant order. Useful as the
    /// `candidate_child_kinds` argument to [`crate::query::
    /// explain_cluster_centroid`] when the caller wants the cleanup-
    /// memory to consider every kind. Replaces the 7-element array
    /// literal that was duplicated across multiple test modules.
    pub const ALL: [CanonicalKind; 7] = [
        CanonicalKind::Decl,
        CanonicalKind::Expr,
        CanonicalKind::Stmt,
        CanonicalKind::Block,
        CanonicalKind::Ref,
        CanonicalKind::Lit,
        CanonicalKind::Op,
    ];
}

/// Lookup interface — every supported language ships an implementation that
/// maps its parser's named kinds to a `CanonicalKind`. Anything not in the
/// table defaults to `Block` (the most-neutral structural carrier — picks
/// up boilerplate without polluting other classes).
pub trait CanonicalKindMap: Send + Sync {
    /// Resolve a parser-given kind name to its canonical role.
    /// Returns `Block` for unknown kinds (forward-compat: a future
    /// tree-sitter version can add kinds without breaking encoders).
    fn lookup(&self, kind: &str) -> CanonicalKind;

    /// Language identifier this map is for (`"rust"`, `"go"`, etc.).
    fn lang(&self) -> &'static str;
}

/// Default fallback when a kind isn't in any specific map.
pub const FALLBACK_KIND: CanonicalKind = CanonicalKind::Block;

pub mod rust;
pub use rust::RustCanonicalMap;

pub mod go;
pub use go::GoCanonicalMap;

pub mod yaml;
pub use yaml::YamlCanonicalMap;

pub mod json;
pub use json::JsonCanonicalMap;

/// Look up a canonical-kind map by language id. Single dispatch point for
/// "I have a parser, I have a language id, give me the right map." Without
/// this helper every caller would hardcode the language→map mapping
/// inline, drifting on every new language addition.
///
/// Returns `None` if the language has no canonical map registered yet —
/// callers can fall back to a Block-defaulting map (`leyline_hdc` doesn't
/// ship one) or skip HDC encoding for that file.
///
/// The match must agree with the language ids that
/// `leyline_lsp::languages::language_id_from_ext` and
/// `leyline_ts::languages::TsLanguage` use, so a `.go` file routed via
/// either registry resolves to `GoCanonicalMap` here.
pub fn select_canonical_map(lang: &str) -> Option<Box<dyn CanonicalKindMap>> {
    match lang {
        "rust" => Some(Box::new(RustCanonicalMap)),
        "go" => Some(Box::new(GoCanonicalMap)),
        "yaml" => Some(Box::new(YamlCanonicalMap)),
        "json" => Some(Box::new(JsonCanonicalMap)),
        _ => None,
    }
}

/// Test invariants every `CanonicalKindMap` impl must satisfy. Centralizes
/// the boilerplate that would otherwise be copied per-language: forward-
/// compat fallback, empty-string fallback, and lang-id identification.
/// Adding a new language map gets these checks for free by calling
/// `assert_canonical_map_baseline(&MyMap, "mylang")`.
#[cfg(test)]
pub fn assert_canonical_map_baseline(m: &dyn CanonicalKindMap, expected_lang: &str) {
    // Forward compat: a future grammar adds a kind we don't know yet.
    // Encoder must keep working — unknown kinds bucket to FALLBACK_KIND
    // (Block) until the map is updated. Pin so a refactor that changed
    // the fallback (e.g. to Stmt) is caught immediately.
    assert_eq!(
        m.lookup("future_unknown_kind"),
        FALLBACK_KIND,
        "{}: unknown kind must fall back to FALLBACK_KIND",
        m.lang(),
    );
    assert_eq!(
        m.lookup(""),
        FALLBACK_KIND,
        "{}: empty kind name must fall back to FALLBACK_KIND",
        m.lang(),
    );

    // Language identity: each map must self-identify so multi-language
    // collections can disambiguate without out-of-band metadata.
    assert_eq!(m.lang(), expected_lang);
}

/// Assert each `(kind_str, expected_canonical)` pair maps as expected.
/// Use to pin a sample of important kinds per language without copying
/// the same `assert_eq!(m.lookup("X"), CanonicalKind::Y)` shape
/// per-language. Diagnostic includes the language id and the kind so a
/// failure tells you exactly which mapping drifted.
#[cfg(test)]
pub fn assert_kinds_match(m: &dyn CanonicalKindMap, pairs: &[(&str, CanonicalKind)]) {
    for (kind, expected) in pairs {
        assert_eq!(
            m.lookup(kind),
            *expected,
            "{}: lookup(\"{kind}\") expected {expected:?}",
            m.lang(),
        );
    }
}

/// Assert that the given probe set covers every variant in
/// `CanonicalKind::ALL` AND that each probe maps to its declared
/// kind. Used by per-language tests to enforce "no canonical role
/// silently dropped from the map" — a refactor that removed all
/// `CanonicalKind::Ref` arms would silently funnel every former-Ref
/// kind into `FALLBACK_KIND` and shift every encoded hypervector
/// that contained the lost role.
#[cfg(test)]
pub fn assert_kinds_cover_all_canonical(
    m: &dyn CanonicalKindMap,
    probes: &[(&str, CanonicalKind)],
) {
    let covered: std::collections::HashSet<CanonicalKind> =
        probes.iter().map(|(_, k)| *k).collect();
    assert_eq!(
        covered.len(),
        CanonicalKind::ALL.len(),
        "{}: probe set must hit every CanonicalKind exactly once (got {} unique)",
        m.lang(),
        covered.len(),
    );
    assert_kinds_match(m, probes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_stable_and_distinct() {
        // The discriminant feeds the hash that produces base hypervectors.
        // Values must never change — they're frozen at the moment of
        // first encoding any corpus. Pin all seven explicitly.
        assert_eq!(CanonicalKind::Decl.discriminant(), 0);
        assert_eq!(CanonicalKind::Expr.discriminant(), 1);
        assert_eq!(CanonicalKind::Stmt.discriminant(), 2);
        assert_eq!(CanonicalKind::Block.discriminant(), 3);
        assert_eq!(CanonicalKind::Ref.discriminant(), 4);
        assert_eq!(CanonicalKind::Lit.discriminant(), 5);
        assert_eq!(CanonicalKind::Op.discriminant(), 6);

        // Distinctness check via the `ALL` constant — also catches a
        // refactor that adds a variant without updating ALL or that
        // collapses two variants to the same discriminant.
        let mut seen = std::collections::HashSet::new();
        for k in CanonicalKind::ALL {
            assert!(seen.insert(k.discriminant()), "duplicate discriminant for {k:?}");
        }
    }

    #[test]
    fn all_constant_in_discriminant_order() {
        // `ALL` is exposed as the canonical "every kind" array used by
        // cleanup-memory consumers (e.g. `explain_cluster_centroid`).
        // Pin the order matches the discriminant assignment so callers
        // that iterate `CanonicalKind::ALL` get a predictable sequence.
        // Also catches a future refactor that adds a variant without
        // appending it here (the length assertion would fail).
        assert_eq!(CanonicalKind::ALL.len(), 7);
        for (i, k) in CanonicalKind::ALL.iter().enumerate() {
            assert_eq!(
                k.discriminant() as usize,
                i,
                "CanonicalKind::ALL[{i}] = {k:?} but discriminant() = {}",
                k.discriminant()
            );
        }
    }

    #[test]
    fn fallback_is_block() {
        // Unknown kinds default to Block by convention. A refactor that
        // changed the fallback (e.g. to Stmt) would silently shift
        // hypervectors for every parser-version-skewed kind. Pin it.
        assert_eq!(FALLBACK_KIND, CanonicalKind::Block);
    }

    #[test]
    fn select_canonical_map_returns_correct_map_for_known_languages() {
        // Single dispatch point. Pin every registered language so a
        // future addition that forgets to wire up a new language fails
        // here. Each entry: (lang_id, expected_self_reported_lang).
        let probes: &[(&str, &str)] = &[
            ("rust", "rust"),
            ("go", "go"),
            ("yaml", "yaml"),
            ("json", "json"),
        ];
        for (lang, expected) in probes {
            let m = select_canonical_map(lang).unwrap_or_else(|| {
                panic!("select_canonical_map returned None for known lang `{lang}`")
            });
            assert_eq!(
                m.lang(),
                *expected,
                "select_canonical_map(`{lang}`) returned a map identifying as `{}`, expected `{expected}`",
                m.lang(),
            );
        }
    }

    #[test]
    fn select_canonical_map_returns_none_for_unknown_languages() {
        // Forward-compat: caller-provided unknown languages must return
        // None (callers fall back to skipping HDC for that file). A
        // refactor that erroneously returned a default map would inject
        // wrong-language hypervectors silently.
        for unknown in ["", "elixir", "ruby", "future_lang"] {
            assert!(
                select_canonical_map(unknown).is_none(),
                "select_canonical_map(`{unknown}`) must return None for unregistered language",
            );
        }
    }
}
