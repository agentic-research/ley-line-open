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

        // Distinctness check — defensive in case a refactor accidentally
        // collapses two variants to the same value.
        let mut seen = std::collections::HashSet::new();
        for k in [
            CanonicalKind::Decl,
            CanonicalKind::Expr,
            CanonicalKind::Stmt,
            CanonicalKind::Block,
            CanonicalKind::Ref,
            CanonicalKind::Lit,
            CanonicalKind::Op,
        ] {
            assert!(seen.insert(k.discriminant()), "duplicate discriminant for {k:?}");
        }
    }

    #[test]
    fn fallback_is_block() {
        // Unknown kinds default to Block by convention. A refactor that
        // changed the fallback (e.g. to Stmt) would silently shift
        // hypervectors for every parser-version-skewed kind. Pin it.
        assert_eq!(FALLBACK_KIND, CanonicalKind::Block);
    }
}
