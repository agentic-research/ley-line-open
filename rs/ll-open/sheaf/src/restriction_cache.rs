//! Restriction-addressed review cache — reusable core (ADR-0031).
//!
//! Graduated out of `tests/restriction_review_real.rs` (bead
//! `ley-line-open-054048`, was `f38a86`) so the git-replay bench
//! (`f3a81e`) and the second review family (`f463aa`) reuse ONE
//! definition of the restriction / review-result / oracle / substrate
//! rather than duplicating it.
//!
//! ## What lives here vs. in the test/bench layer
//!
//! This module is **parser-independent** and ships in production. It
//! operates on already-extracted fact rows — the `node_defs` /
//! `node_refs` / `_imports` columns the live pipeline persists — modeled
//! as [`DefRow`] / [`RefRow`] / [`ImportRow`]. A production consumer
//! would populate a [`FactSubstrate`] from the daemon's SQL fact tables;
//! `leyline-sheaf` stays free of `leyline-ts` / `tree-sitter` (they are
//! dev/bench-deps only — see `Cargo.toml`). The **extraction** that turns
//! Rust source into these rows via `leyline_ts::refs::extract_rust` lives
//! in `tests/common/mod.rs`, shared by tests and benches, and is a
//! stand-in for the daemon's fact tables.
//!
//! ## Claim under test (falsifiable)
//!
//! A cached expensive review result can be safely reused when its cheap
//! fact-specific restriction hash is unchanged, even when the whole-object
//! content hash changed.
//!
//! One review family: the CALL-TARGET review of a function `F` ("what does
//! `F` call, and where does each call resolve?").
//!
//! Three artifacts, kept structurally separate:
//!
//! 1. RESTRICTION (cheap, [`restriction_for_call_target`]): a hash over a
//!    sound superset of the review's INPUT rows — `F`'s container
//!    identity, the sorted `(token, qualifier)` of `node_refs` rows whose
//!    container is `F`, the `(alias, path)` import rows of `F`'s file whose
//!    alias any target token/qualifier names, and the `node_defs` rows
//!    `F`'s target tokens index to (a token-indexed point lookup,
//!    cross-file). It never runs resolution.
//! 2. REVIEW RESULT (expensive, [`review_call_targets`]): the resolved
//!    call graph — an unindexed cross-corpus JOIN over all `node_defs`
//!    plus the import surface, producing [`ResolvedEdge`]s.
//! 3. ORACLE: run the review on both versions and compare outputs — never
//!    consult the restriction.
//!
//! ## Container identity (bead `ley-line-open-054048`, the re-key)
//!
//! The restriction's FIRST hashed field is `F`'s **container identity**.
//! Which string that is comes from [`ContainerKeying`], decided at
//! extraction time — this module is keying-agnostic and hashes whatever
//! string it is given. The keying choice is the load-bearing deployment
//! precondition (ADR-0031 caveat #1): a POSITIONAL id shifts on any line
//! change above `F` and degenerates the restriction to whole-file
//! sensitivity, whereas a reflow-invariant node_hash-style identity
//! survives edits elsewhere. See [`ContainerKeying`].

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Unit separator — cannot occur in tokens, paths, or grammar kinds, so
/// hashed row boundaries are unambiguous.
pub const US: char = '\u{1f}';

// ---------------------------------------------------------------------------
// Fact substrate: the node_defs / node_refs / _imports rows the live
// pipeline emits. Parser-independent — a production consumer fills these
// from SQL; tests fill them via tree-sitter (see tests/common).
// ---------------------------------------------------------------------------

/// A `node_defs` row: a definition token, the file it lives in, and its κ
/// canonical kind.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DefRow {
    pub token: String,
    pub source_id: String,
    pub kind: String,
}

/// A `node_refs` row: a reference token, its optional syntactic qualifier,
/// the identity of the enclosing container (see [`ContainerKeying`]), and
/// its file.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RefRow {
    pub token: String,
    pub qualifier: Option<String>,
    pub container: Option<String>,
    pub source_id: String,
}

/// An `_imports` row: an alias→path mapping scoped to a file.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ImportRow {
    pub alias: String,
    pub path: String,
    pub source_id: String,
}

/// How the container identity string carried by [`RefRow::container`] was
/// derived. The restriction hashes the string verbatim; the SOUNDNESS and
/// USEFULNESS of restriction-addressing hinge on which of these it is.
///
/// - [`ContainerKeying::Name`] — the bare `fn:score`. What the original
///   experiment used. Stable under position and body, but not unique when
///   two functions share a name in different scopes.
/// - [`ContainerKeying::Positional`] — the daemon's actual
///   `container_node_id`: a constructed AST path
///   (`.../function_item_1`) whose same-kind sibling suffix SHIFTS when a
///   function is inserted above `F`. Keying on this degenerates the
///   restriction to whole-file sensitivity (still sound, loses true skips)
///   — this is ADR-0031 caveat #1, reproduced RED by the
///   `stable_identity_survives_insert_above` fixture.
/// - [`ContainerKeying::Stable`] — a reflow-invariant node_hash-style
///   identity (ADR-0027). Position-invariant AND body-invariant, so it
///   survives insert-above (the fix, GREEN) without defeating the
///   body-only true-skips (local-rename, body-arith). The test-layer
///   stand-in hashes `F`'s SIGNATURE subtree (function minus its body
///   block); production reads the fold's `node_hash` for the function's
///   declaration node. See `tests/common`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContainerKeying {
    Name,
    Positional,
    Stable,
}

/// The extracted facts for one corpus, plus the indexes a daemon query
/// would hit. Built via [`FactSubstrate::from_rows`].
pub struct FactSubstrate {
    pub defs: Vec<DefRow>,
    pub refs: Vec<RefRow>,
    pub imports: Vec<ImportRow>,
    /// token → indices into `defs`. Models the indexed point lookup a
    /// daemon-side `node_defs(token)` query performs.
    def_index: BTreeMap<String, Vec<usize>>,
    /// container → indices into `refs`. Models the indexed per-container
    /// `node_refs` lookup.
    refs_by_container: BTreeMap<String, Vec<usize>>,
    /// function name → its container identity string under the active
    /// keying. Lets a driver resolve "review the function named `score`"
    /// to the id the refs are grouped by. Keyed by bare name for the
    /// experiment; the daemon's node_hash identity is globally unique.
    container_by_fn: BTreeMap<String, String>,
}

impl FactSubstrate {
    /// Build the substrate and its indexes from already-extracted rows.
    /// `defs` / `refs` / `imports` should already be de-duplicated as sets
    /// (multiplicity of identical rows does not enter the restriction or
    /// the review). `container_by_fn` maps each function's name to the
    /// container identity string its refs carry.
    pub fn from_rows(
        defs: Vec<DefRow>,
        refs: Vec<RefRow>,
        imports: Vec<ImportRow>,
        container_by_fn: BTreeMap<String, String>,
    ) -> Self {
        let mut def_index: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, d) in defs.iter().enumerate() {
            def_index.entry(d.token.clone()).or_default().push(i);
        }
        let mut refs_by_container: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, r) in refs.iter().enumerate() {
            if let Some(c) = &r.container {
                refs_by_container.entry(c.clone()).or_default().push(i);
            }
        }
        FactSubstrate {
            defs,
            refs,
            imports,
            def_index,
            refs_by_container,
            container_by_fn,
        }
    }

    /// Resolve a function NAME to its container identity string under the
    /// keying this substrate was built with. `None` if no such function
    /// was extracted.
    pub fn container_for_fn(&self, name: &str) -> Option<&str> {
        self.container_by_fn.get(name).map(String::as_str)
    }
}

fn index_slice<'a>(index: &'a BTreeMap<String, Vec<usize>>, key: &str) -> &'a [usize] {
    index.get(key).map(Vec::as_slice).unwrap_or(&[])
}

// ---------------------------------------------------------------------------
// 1. RESTRICTION — cheap projection hash over the review's input rows.
// ---------------------------------------------------------------------------

/// Hash of the sound superset of facts the call-target review of
/// `container` depends on. Indexed lookups only; no resolution logic.
/// `rows_touched` counts substrate rows read — the cost proxy shared with
/// [`review_call_targets`].
pub fn restriction_for_call_target(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> [u8; 32] {
    let mut buf = String::with_capacity(512);
    push_row(&mut buf, &["container", container]);

    // (a) F's own call-target rows: sorted (token, qualifier). BTreeSet
    // storage order makes the indexed slice already sorted.
    let mut tokens: BTreeSet<&str> = BTreeSet::new();
    let mut qualifiers: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    for &i in index_slice(&sub.refs_by_container, container) {
        let r = &sub.refs[i];
        *rows_touched += 1;
        push_row(
            &mut buf,
            &["ref", &r.token, r.qualifier.as_deref().unwrap_or("")],
        );
        tokens.insert(&r.token);
        if let Some(q) = &r.qualifier {
            qualifiers.insert(q);
        }
        files.insert(&r.source_id);
    }

    // (b) the relevant import surface: (alias, path) rows in F's file whose
    // alias one of F's target tokens or qualifiers names.
    for imp in &sub.imports {
        *rows_touched += 1;
        if files.contains(imp.source_id.as_str())
            && (tokens.contains(imp.alias.as_str()) || qualifiers.contains(imp.alias.as_str()))
        {
            push_row(&mut buf, &["import", &imp.alias, &imp.path]);
        }
    }

    // (c) the def rows the target tokens can resolve to — token-indexed
    // point lookups, cross-file. This is what makes the restriction a sound
    // superset for a CROSS-ITEM review: a dep-side def change must
    // invalidate even though F's own file is byte-identical.
    for token in &tokens {
        if let Some(rows) = sub.def_index.get(*token) {
            for &i in rows {
                let d = &sub.defs[i];
                *rows_touched += 1;
                push_row(&mut buf, &["def", &d.token, &d.source_id, &d.kind]);
            }
        }
    }

    Sha256::digest(buf.as_bytes()).into()
}

/// Append one US-delimited, newline-terminated canonical row.
fn push_row(buf: &mut String, fields: &[&str]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            buf.push(US);
        }
        buf.push_str(f);
    }
    buf.push('\n');
}

// ---------------------------------------------------------------------------
// 2. REVIEW RESULT — the expensive resolved call graph.
// ---------------------------------------------------------------------------

/// One resolved call edge of F: which def rows the target token joins to
/// across the whole corpus, and through which import.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ResolvedEdge {
    pub token: String,
    pub qualifier: Option<String>,
    pub via_import: Option<String>,
    pub candidates: Vec<DefRow>,
}

/// The expensive path: for every call-target row of `container`, resolve
/// the import it travels through (file-scoped scan) and JOIN against ALL
/// `node_defs` rows in the corpus (unindexed scan — the honest cost of
/// cross-item resolution, and still only a stand-in for a real review,
/// which would be an analysis or an LLM pass on top of these edges; the
/// measured gap is a lower bound).
pub fn review_call_targets(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> BTreeSet<ResolvedEdge> {
    let mut out = BTreeSet::new();
    for &i in index_slice(&sub.refs_by_container, container) {
        let r = &sub.refs[i];
        *rows_touched += 1;

        let mut via_import = None;
        for imp in &sub.imports {
            *rows_touched += 1;
            if via_import.is_none()
                && imp.source_id == r.source_id
                && (imp.alias == r.token || r.qualifier.as_deref() == Some(imp.alias.as_str()))
            {
                via_import = Some(imp.path.clone());
            }
        }

        let mut candidates = Vec::new();
        for d in &sub.defs {
            *rows_touched += 1;
            if d.token == r.token {
                candidates.push(d.clone());
            }
        }
        candidates.sort();

        out.insert(ResolvedEdge {
            token: r.token.clone(),
            qualifier: r.qualifier.clone(),
            via_import,
            candidates,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Baseline policies + evaluation scaffolding.
// ---------------------------------------------------------------------------

/// Whole-object CAS hash: the byte hash of the WHOLE CORPUS (the only
/// per-object hash that is sound for a cross-file review). Parser-free.
pub fn whole_object_hash(corpus: &[(String, String)]) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, src) in corpus {
        h.update(format!("{path}{US}{src}\n").as_bytes());
    }
    h.finalize().into()
}

/// The cache policies compared per fixture.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Policy {
    WholeObject,
    AstShape,
    Restriction,
}

/// One fixture's outcome: whether the review RESULT changed (oracle) and,
/// per policy, whether that policy would SKIP (reuse the cache).
pub struct FixtureResult {
    pub name: String,
    pub review_changed: bool,
    pub skips: BTreeMap<Policy, bool>,
}

/// Aggregate skip quality for a policy across fixtures.
#[derive(Default)]
pub struct PolicyStats {
    /// Skipped when the review actually changed — unsound.
    pub false_skips: usize,
    /// Skipped when the review was genuinely unchanged — the useful win.
    pub sound_skips: usize,
}

/// Tally false-skips vs sound-skips for one policy over a fixture set.
pub fn stats(results: &[FixtureResult], policy: Policy) -> PolicyStats {
    let mut s = PolicyStats::default();
    for r in results {
        if r.skips[&policy] {
            if r.review_changed {
                s.false_skips += 1;
            } else {
                s.sound_skips += 1;
            }
        }
    }
    s
}
