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
//! `leyline-cache` stays free of `leyline-ts` / `tree-sitter` (they are
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
//!
//! ## The key (bead `ley-line-open-754aa1`, ADR-0032 D2)
//!
//! A restriction key is a [`PartitionSpec`] `RowSet` fold over the closure
//! rows, never a hash of their concatenation:
//!
//! ```text
//! rowAddr(r)  = BLAKE3_derive_key(ROW_CONTEXT, n ‖ (|fᵢ| ‖ fᵢ)…)      fields fᵢ of r
//! key(F)      = A({RowSet, scheme, params = parserId, canon 1},
//!                 { (rowAddr(r), a = kind(r), b = 0) : r ∈ closure(F) })
//! ```
//!
//! Every field is length-prefixed and the field count leads, so a row's
//! address commits to its field split; the fold commits to the multiset of
//! `(rowAddr, kind)` pairs, so it commits to the row split. An optional
//! field carries a presence byte, so `None` and `Some("")` differ. The
//! scheme names the review family and its resolution rule
//! ([`CALL_TARGET_SCHEME`], [`PUBLIC_API_SCHEME`]), and `parserId` in params
//! binds the derivation the rows were extracted under, so a key computed
//! under one extraction epoch never matches one computed under another.
//!
//! The pre-754aa1 key joined fields with U+001F and rows with `\n`, then
//! hashed the string. POSIX paths may contain both bytes, so two different
//! fact sets could share a key (finding `80f037` F5). The property tests at
//! the bottom of this file show the new key separating every pair the old
//! one collides.

use leyline_core::{ContentAddressed, Domain, Entry, Hash, PartitionSpec};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

/// `derive_key` context of one closure row's address. Protocol-visible:
/// changing the row encoding is a `v2`, not an edit.
pub const ROW_CONTEXT: &str = "leyline/restriction/v1";

/// Scheme of the call-target family. `token-eq` names the resolution rule
/// (a def row joins a ref row by bare token equality); a scope-aware resolver
/// is a different scheme, not a new version of this one.
pub const CALL_TARGET_SCHEME: &str = "leyline/restriction/call-target/token-eq/v1";

/// Scheme of the public-API family (same `token-eq` resolution rule).
pub const PUBLIC_API_SCHEME: &str = "leyline/restriction/public-api/token-eq/v1";

/// Scheme of [`whole_object_hash`], the sound whole-corpus baseline.
pub const WHOLE_OBJECT_SCHEME: &str = "leyline/restriction/whole-object/v1";

/// Scheme of [`whole_input_key`], the whole-input ablation arm.
pub const WHOLE_INPUT_SCHEME: &str = "leyline/restriction/whole-input/v1";

/// What one closure row records. The value is the row's `a` framing in the
/// fold, so it is protocol-visible: append, never renumber.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RowKind {
    /// The reviewed function's container identity.
    Container = 0,
    /// A `node_refs` row contained in the function: `(token, qualifier?)`.
    Ref = 1,
    /// An `_imports` row of the function's file whose alias a ref names.
    Import = 2,
    /// A `node_defs` row a ref token resolves to: `(token, source, kind)`.
    Def = 3,
    /// The public symbol's `(token, kind)`.
    Api = 4,
    /// The public symbol's receiver qualifier (optional).
    Qualifier = 5,
    /// The public symbol's visibility.
    Visibility = 6,
    /// The signature subtree's stable identity.
    Signature = 7,
    /// One type token the signature names.
    SigType = 8,
    /// A `node_defs` row a signature type token resolves to.
    TypeDef = 9,
}

/// Where a closure row came from. It never enters the key; it lets a test
/// drop exactly one dependency from a closure and watch the cache go unsound
/// (the closure-completeness mutation test).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Provenance {
    /// The query itself: the container id, or a field of the [`ApiDefRow`].
    Query,
    /// `FactSubstrate::refs[i]`.
    Ref(usize),
    /// `FactSubstrate::imports[i]`.
    Import(usize),
    /// `FactSubstrate::defs[i]`.
    Def(usize),
    /// The `i`-th distinct signature type token of the [`ApiDefRow`].
    SigType(usize),
}

/// One row of a review's input closure: the section the key is computed
/// over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub kind: RowKind,
    pub fields: Vec<Vec<u8>>,
    pub provenance: Provenance,
}

impl Row {
    fn new(kind: RowKind, provenance: Provenance, fields: &[&[u8]]) -> Self {
        Row {
            kind,
            fields: fields.iter().map(|f| f.to_vec()).collect(),
            provenance,
        }
    }

    /// The row's leaf address: keyed, field count first, every field
    /// length-prefixed.
    pub fn address(&self) -> Hash {
        // `new_derive_key` hashes the context string on every call; the
        // keyed state is the same each time, so clone it instead.
        static ROW_HASHER: LazyLock<blake3::Hasher> =
            LazyLock::new(|| blake3::Hasher::new_derive_key(ROW_CONTEXT));
        let mut h = ROW_HASHER.clone();
        h.update(&(self.fields.len() as u64).to_le_bytes());
        for f in &self.fields {
            h.update(&(f.len() as u64).to_le_bytes());
            h.update(f);
        }
        Hash::from_bytes(*h.finalize().as_bytes())
    }
}

/// An optional field: a presence byte, then the value. `None` and
/// `Some("")` encode differently.
fn opt_field(v: Option<&str>) -> Vec<u8> {
    match v {
        None => vec![0],
        Some(s) => {
            let mut out = Vec::with_capacity(1 + s.len());
            out.push(1);
            out.extend_from_slice(s.as_bytes());
            out
        }
    }
}

/// The restriction key over a closure: the `RowSet` fold of ADR-0032 D2,
/// `scheme` naming the family, `parser_id` in params. A function of the
/// multiset of `(kind, fields)` rows; [`Row::provenance`] is not hashed.
pub fn restriction_key(scheme: &str, parser_id: Hash, rows: &[Row]) -> Hash {
    let entries: Vec<Entry> = rows
        .iter()
        .map(|r| Entry {
            addr: r.address(),
            a: r.kind as u64,
            b: 0,
        })
        .collect();
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: scheme.to_string(),
        params: parser_id.as_bytes().to_vec(),
        canon_version: 1,
    }
    .address(&entries)
}

/// The restriction-key construction (ADR-0031 as amended, ADR-0047 §3: the
/// first shipped lawful `Ref<Restriction>`). Its fold is
/// [`restriction_key`]; nothing moves.
pub enum Restriction {}

/// What a restriction key is a function of: the family's scheme, the parser
/// identity the facts were extracted under, and the closure's rows.
#[derive(Clone, Copy, Debug)]
pub struct RestrictionWitness<'w> {
    pub scheme: &'w str,
    pub parser_id: Hash,
    pub rows: &'w [Row],
}

impl leyline_core::Construction for Restriction {
    const NAME: &'static str = "restriction";
    type Witness<'w> = RestrictionWitness<'w>;
    fn fold(w: RestrictionWitness<'_>) -> Hash {
        restriction_key(w.scheme, w.parser_id, w.rows)
    }
}

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
    /// file → indices into `imports`. Models the indexed per-file
    /// `_imports` lookup, so the closure reads only the imports of `F`'s
    /// files rather than scanning the corpus.
    imports_by_file: BTreeMap<String, Vec<usize>>,
    /// function name → its container identity string under the active
    /// keying. Lets a driver resolve "review the function named `score`"
    /// to the id the refs are grouped by. Keyed by bare name for the
    /// experiment; the daemon's node_hash identity is globally unique.
    container_by_fn: BTreeMap<String, String>,
    /// The derivation the rows were extracted under (`Head.parserId` in
    /// the daemon). Every key over this substrate binds it.
    parser_id: Hash,
}

impl FactSubstrate {
    /// Build the substrate and its indexes from already-extracted rows.
    /// `defs` / `refs` / `imports` should already be de-duplicated as sets
    /// (multiplicity of identical rows does not enter the restriction or
    /// the review). `container_by_fn` maps each function's name to the
    /// container identity string its refs carry. `parser_id` is the
    /// derivation that produced the rows.
    pub fn from_rows(
        defs: Vec<DefRow>,
        refs: Vec<RefRow>,
        imports: Vec<ImportRow>,
        container_by_fn: BTreeMap<String, String>,
        parser_id: Hash,
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
        let mut imports_by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, imp) in imports.iter().enumerate() {
            imports_by_file
                .entry(imp.source_id.clone())
                .or_default()
                .push(i);
        }
        FactSubstrate {
            defs,
            refs,
            imports,
            def_index,
            refs_by_container,
            imports_by_file,
            container_by_fn,
            parser_id,
        }
    }

    /// The derivation the rows were extracted under.
    pub fn parser_id(&self) -> Hash {
        self.parser_id
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

/// The input closure of the call-target review of `container`: the sound
/// superset of facts the review depends on. Indexed lookups only; no
/// resolution logic. `rows_touched` counts substrate rows read — the cost
/// proxy shared with [`review_call_targets`].
pub fn call_target_closure(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> Vec<Row> {
    let mut rows = vec![Row::new(
        RowKind::Container,
        Provenance::Query,
        &[container.as_bytes()],
    )];

    // (a) F's own call-target rows: (token, qualifier).
    let mut tokens: BTreeSet<&str> = BTreeSet::new();
    let mut qualifiers: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    for &i in index_slice(&sub.refs_by_container, container) {
        let r = &sub.refs[i];
        *rows_touched += 1;
        rows.push(Row::new(
            RowKind::Ref,
            Provenance::Ref(i),
            &[r.token.as_bytes(), &opt_field(r.qualifier.as_deref())],
        ));
        tokens.insert(&r.token);
        if let Some(q) = &r.qualifier {
            qualifiers.insert(q);
        }
        files.insert(&r.source_id);
    }

    // (b) the relevant import surface: (alias, path) rows in F's file whose
    // alias one of F's target tokens or qualifiers names. File-indexed, so
    // only the imports of F's own files are read.
    for &i in files
        .iter()
        .flat_map(|f| index_slice(&sub.imports_by_file, f))
    {
        let imp = &sub.imports[i];
        *rows_touched += 1;
        if tokens.contains(imp.alias.as_str()) || qualifiers.contains(imp.alias.as_str()) {
            rows.push(Row::new(
                RowKind::Import,
                Provenance::Import(i),
                &[imp.alias.as_bytes(), imp.path.as_bytes()],
            ));
        }
    }

    // (c) the def rows the target tokens can resolve to — token-indexed
    // point lookups, cross-file. This is what makes the restriction a sound
    // superset for a CROSS-ITEM review: a dep-side def change must
    // invalidate even though F's own file is byte-identical.
    for token in &tokens {
        for &i in index_slice(&sub.def_index, token) {
            let d = &sub.defs[i];
            *rows_touched += 1;
            rows.push(Row::new(
                RowKind::Def,
                Provenance::Def(i),
                &[
                    d.token.as_bytes(),
                    d.source_id.as_bytes(),
                    d.kind.as_bytes(),
                ],
            ));
        }
    }

    rows
}

/// The call-target restriction key of `container`: [`restriction_key`] over
/// [`call_target_closure`], under [`CALL_TARGET_SCHEME`] and the substrate's
/// `parserId`.
pub fn restriction_for_call_target(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> Hash {
    restriction_key(
        CALL_TARGET_SCHEME,
        sub.parser_id,
        &call_target_closure(sub, container, rows_touched),
    )
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

// ===========================================================================
// SECOND REVIEW FAMILY (bead ley-line-open-0567e8 / f463aa): PUBLIC-API
// review, keyed on node_defs rather than node_refs.
//
// The call-target family above proves restriction-addressing for a review
// that reads a function's CALL SITES (node_refs contained in `F`). This
// family proves the pattern GENERALIZES to a review that reads a
// definition's PUBLIC SURFACE (the node_def, its visibility, and the
// SIGNATURE types it names) — a different projection of the same substrate,
// with its OWN restriction key and its OWN false-skip measurement.
//
// The two families are genuinely distinct, not a relabeling: a body-only
// edit (rename a private helper the public fn CALLS) invalidates the
// call-target restriction but NOT the public-API restriction, because the
// public-API review never observes the body. That contrast is the proof the
// per-family restriction is scoped to what its review actually reads.
// ===========================================================================

/// The signature-scoped facts a PUBLIC-API review of one definition
/// observes — a `node_defs` row enriched with the fields a public-surface
/// review reads. Parser-independent; the extraction layer (tests/common)
/// fills it from a `function_item`'s DECLARATION subtree (never its body).
///
/// - `token` / `qualifier` / `kind` — the def's identity as a public symbol
///   (`qualifier` carries the receiver for an `impl` method, e.g. `Widget`
///   in `Widget::area`).
/// - `is_public` — visibility. A `pub` → private flip removes the symbol
///   from the public surface, so the review output changes and the
///   restriction MUST see it.
/// - `sig_identity` — the reflow- AND body-invariant node_hash of the
///   function's SIGNATURE subtree (declaration minus `body` block; see
///   `tests/common::signature_node_hash`). This is the stable identity
///   ADR-0031 caveat #1 requires, scoped so that body edits do not move it.
/// - `sig_type_tokens` — the param/return TYPE identifiers the signature
///   names (`i64`, `Widget`, …), sorted+deduped. These are what the review
///   resolves across the corpus; a dep-side type-def change reaches the
///   review through them, which is what makes this restriction span
///   MULTIPLE objects (the sheaf-shaped property).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ApiDefRow {
    pub token: String,
    pub qualifier: Option<String>,
    pub kind: String,
    pub source_id: String,
    pub is_public: bool,
    pub sig_identity: [u8; 32],
    pub sig_type_tokens: Vec<String>,
}

/// RESTRICTION (cheap) for the PUBLIC-API review of `api`: the
/// [`PUBLIC_API_SCHEME`] key over [`public_api_closure`].
pub fn restriction_for_public_api(
    sub: &FactSubstrate,
    api: &ApiDefRow,
    rows_touched: &mut u64,
) -> Hash {
    restriction_key(
        PUBLIC_API_SCHEME,
        sub.parser_id,
        &public_api_closure(sub, api, rows_touched),
    )
}

/// The input closure of the PUBLIC-API review of `api`.
///
/// A sound superset of the review's INPUT rows:
///   (a) the def's identity as a public symbol — `token`, `qualifier`,
///       `kind`, and `is_public` (visibility);
///   (b) the signature-scoped stable identity `sig_identity` — the
///       declaration subtree hash, reflow- and body-invariant, so
///       whitespace and body edits leave it fixed;
///   (c) the sorted signature TYPE tokens; and
///   (d) — load-bearingly, cross-object — the `node_defs` rows those type
///       tokens resolve to across the WHOLE corpus, via the token-indexed
///       point lookup. This is the multi-object span: a dep-side type-def
///       rename in another file must invalidate even though `api`'s own
///       file is byte-identical.
///
/// Indexed lookups only; no resolution logic. Never touches the body's
/// `node_refs`. `rows_touched` is the cost proxy shared with
/// [`review_public_api`].
pub fn public_api_closure(
    sub: &FactSubstrate,
    api: &ApiDefRow,
    rows_touched: &mut u64,
) -> Vec<Row> {
    // (a) the public symbol's identity + (b) its stable signature identity.
    let q = Provenance::Query;
    let mut rows = vec![
        Row::new(
            RowKind::Api,
            q,
            &[api.token.as_bytes(), api.kind.as_bytes()],
        ),
        Row::new(
            RowKind::Qualifier,
            q,
            &[&opt_field(api.qualifier.as_deref())],
        ),
        Row::new(RowKind::Visibility, q, &[&[u8::from(api.is_public)]]),
        Row::new(RowKind::Signature, q, &[&api.sig_identity]),
    ];

    // (c) the signature type tokens, deduped: a set, like the review reads.
    let type_tokens: BTreeSet<&str> = api.sig_type_tokens.iter().map(String::as_str).collect();
    for (i, t) in type_tokens.iter().enumerate() {
        rows.push(Row::new(
            RowKind::SigType,
            Provenance::SigType(i),
            &[t.as_bytes()],
        ));
    }

    // (d) the def rows those signature types resolve to — token-indexed
    // point lookups, cross-file. The sound-superset arm that makes this a
    // CROSS-ITEM review: a signature-named type whose def changes in a
    // different file must invalidate `api`'s cached public surface.
    for token in &type_tokens {
        for &i in index_slice(&sub.def_index, token) {
            let d = &sub.defs[i];
            *rows_touched += 1;
            rows.push(Row::new(
                RowKind::TypeDef,
                Provenance::Def(i),
                &[
                    d.token.as_bytes(),
                    d.source_id.as_bytes(),
                    d.kind.as_bytes(),
                ],
            ));
        }
    }

    rows
}

/// The resolved public surface of a definition — the [`review_public_api`]
/// OUTPUT. Structurally DISTINCT from the restriction hash: it carries the
/// resolved type defs, not a digest.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublicSurface {
    /// `false` when the def is not `pub` — it contributes nothing to the
    /// public surface, and the resolved types are not even computed.
    pub exported: bool,
    pub token: String,
    pub qualifier: Option<String>,
    pub kind: String,
    /// For each signature type token, the def rows it resolves to across
    /// the whole corpus (empty for a primitive like `i64`, or an
    /// unresolved/renamed dep type).
    pub resolved_types: Vec<(String, Vec<DefRow>)>,
}

/// 2. REVIEW RESULT (expensive, DISTINCT) for the PUBLIC-API review.
///
/// If `api` is not public, the surface is empty (early out — matching a real
/// review that skips non-exported symbols). Otherwise, for every signature
/// type token, JOIN against ALL `node_defs` rows in the corpus (unindexed
/// scan — the honest cost of cross-item type resolution, and still only a
/// stand-in for a real public-API review, which would build docs / an ABI
/// descriptor / a semver-diff on top of these resolved types; the measured
/// gap is a lower bound). Never equals the restriction: it returns the
/// resolved [`PublicSurface`], not a hash.
pub fn review_public_api(
    sub: &FactSubstrate,
    api: &ApiDefRow,
    rows_touched: &mut u64,
) -> PublicSurface {
    if !api.is_public {
        return PublicSurface {
            exported: false,
            token: api.token.clone(),
            qualifier: api.qualifier.clone(),
            kind: api.kind.clone(),
            resolved_types: Vec::new(),
        };
    }

    let type_tokens: BTreeSet<&str> = api.sig_type_tokens.iter().map(String::as_str).collect();
    let mut resolved_types = Vec::new();
    for token in &type_tokens {
        let mut candidates = Vec::new();
        for d in &sub.defs {
            *rows_touched += 1;
            if &d.token == token {
                candidates.push(d.clone());
            }
        }
        candidates.sort();
        resolved_types.push((token.to_string(), candidates));
    }

    PublicSurface {
        exported: true,
        token: api.token.clone(),
        qualifier: api.qualifier.clone(),
        kind: api.kind.clone(),
        resolved_types,
    }
}

// ---------------------------------------------------------------------------
// Baseline policies + evaluation scaffolding.
// ---------------------------------------------------------------------------

/// Whole-object CAS address of the WHOLE CORPUS (the only per-object address
/// that is sound for a cross-file review). Parser-free: a `RowSet` fold
/// under [`WHOLE_OBJECT_SCHEME`] with one row `(path, σ(source))` per file,
/// so it names the same information as a `treeRoot` over the corpus.
pub fn whole_object_hash(corpus: &[(String, String)]) -> Hash {
    let rows: Vec<Row> = corpus
        .iter()
        .map(|(path, src)| {
            Row::new(
                RowKind::Container,
                Provenance::Query,
                &[path.as_bytes(), src.as_bytes().hash().as_bytes()],
            )
        })
        .collect();
    restriction_key(WHOLE_OBJECT_SCHEME, Hash::ZERO, &rows)
}

/// The whole-input key of one review: a `RowSet` fold over the input tree's
/// root and the review target, under [`WHOLE_INPUT_SCHEME`] with `parserId`
/// in params. It is the tightest key a cache can build without knowing the
/// review's closure: sound, and invalidated by every edit anywhere in the
/// tree.
pub fn whole_input_key(tree_root: Hash, parser_id: Hash, target: &str) -> Hash {
    let target_row = Row::new(RowKind::Container, Provenance::Query, &[target.as_bytes()]);
    PartitionSpec {
        domain: Domain::RowSet,
        scheme: WHOLE_INPUT_SCHEME.to_string(),
        params: parser_id.as_bytes().to_vec(),
        canon_version: 1,
    }
    .address(&[
        Entry {
            addr: tree_root,
            a: 0,
            b: 0,
        },
        Entry {
            addr: target_row.address(),
            a: 1,
            b: 0,
        },
    ])
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

#[cfg(test)]
mod key_tests {
    use super::*;

    fn pid(b: u8) -> Hash {
        Hash::from_bytes([b; 32])
    }

    fn row(kind: RowKind, fields: &[&str]) -> Row {
        Row {
            kind,
            fields: fields.iter().map(|f| f.as_bytes().to_vec()).collect(),
            provenance: Provenance::Query,
        }
    }

    fn key(rows: &[Row]) -> Hash {
        restriction_key(CALL_TARGET_SCHEME, pid(1), rows)
    }

    fn hex(h: Hash) -> String {
        h.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Known-answer pin: the row encoding and the fold are protocol-visible.
    /// If this moves, every persisted restriction key is invalidated, which
    /// is a `v2` of [`ROW_CONTEXT`] or the scheme, not a refactor.
    #[test]
    fn restriction_key_pin() {
        let rows = [
            row(RowKind::Container, &["h:00"]),
            row(RowKind::Def, &["score", "src/a.rs", "function"]),
        ];
        assert_eq!(
            hex(key(&rows)),
            "1ef07978fae8688269049895bc8913a5bf89abdfe3babf88a84fc248b9e55709",
            "restriction key preimage moved"
        );
    }

    /// The row address is keyed, not a plain BLAKE3 of the fields.
    #[test]
    fn row_address_is_domain_separated() {
        let r = row(RowKind::Ref, &["a"]);
        let mut plain = blake3::Hasher::new();
        plain.update(&1u64.to_le_bytes());
        plain.update(&1u64.to_le_bytes());
        plain.update(b"a");
        assert_ne!(r.address().as_bytes(), plain.finalize().as_bytes());
        assert_ne!(r.address(), b"a".hash());
    }

    /// Field boundaries are committed: ("ab","c") and ("a","bc") concatenate
    /// identically, and so do [] and [""].
    #[test]
    fn row_address_commits_the_field_split() {
        assert_ne!(
            row(RowKind::Def, &["ab", "c"]).address(),
            row(RowKind::Def, &["a", "bc"]).address()
        );
        assert_ne!(
            row(RowKind::Def, &[]).address(),
            row(RowKind::Def, &[""]).address()
        );
        assert_ne!(
            row(RowKind::Def, &["", ""]).address(),
            row(RowKind::Def, &[""]).address()
        );
    }

    /// The pre-754aa1 key forged row boundaries with a newline inside a path.
    /// One row whose path holds `\n` and U+001F is not two rows.
    #[test]
    fn a_path_cannot_forge_a_row_boundary() {
        let two = [
            row(RowKind::Import, &["a", "p"]),
            row(RowKind::Import, &["b", "q"]),
        ];
        let one = [row(RowKind::Import, &["a", "p\nimport\u{1f}b\u{1f}q"])];
        assert_eq!(
            legacy::key(&legacy::from_rows(&two)),
            legacy::key(&legacy::from_rows(&one)),
            "the old key collides on this pair"
        );
        assert_ne!(key(&two), key(&one));
    }

    #[test]
    fn opt_field_separates_none_from_empty() {
        assert_ne!(opt_field(None), opt_field(Some("")));
        assert_eq!(opt_field(None), vec![0]);
        assert_eq!(opt_field(Some("ab")), vec![1, b'a', b'b']);
    }

    #[test]
    fn restriction_key_commits_scheme_parser_and_kind() {
        let rows = [row(RowKind::Ref, &["t"])];
        assert_ne!(
            restriction_key(CALL_TARGET_SCHEME, pid(1), &rows),
            restriction_key(PUBLIC_API_SCHEME, pid(1), &rows),
            "the family is committed"
        );
        assert_ne!(
            restriction_key(CALL_TARGET_SCHEME, pid(1), &rows),
            restriction_key(CALL_TARGET_SCHEME, pid(2), &rows),
            "parserId is committed"
        );
        assert_ne!(
            key(&[row(RowKind::Ref, &["t"])]),
            key(&[row(RowKind::Import, &["t"])]),
            "the row kind is committed"
        );
    }

    /// A closure is a multiset: enumeration order is erased, multiplicity
    /// and provenance-free content are not.
    #[test]
    fn restriction_key_is_a_multiset_function_of_rows() {
        let a = row(RowKind::Ref, &["a"]);
        let b = row(RowKind::Def, &["b", "f", "k"]);
        assert_eq!(key(&[a.clone(), b.clone()]), key(&[b.clone(), a.clone()]));
        assert_ne!(key(std::slice::from_ref(&a)), key(&[a.clone(), a.clone()]));
        let mut moved = a.clone();
        moved.provenance = Provenance::Ref(7);
        assert_eq!(key(&[a]), key(&[moved]), "provenance is not hashed");
    }

    #[test]
    fn whole_object_hash_commits_paths_and_bytes() {
        let c = |p: &str, s: &str| vec![(p.to_string(), s.to_string())];
        let base = whole_object_hash(&c("a.rs", "fn a() {}"));
        assert_ne!(base, whole_object_hash(&c("b.rs", "fn a() {}")));
        assert_ne!(base, whole_object_hash(&c("a.rs", "fn b() {}")));
        assert_ne!(base, whole_object_hash(&[]));
        let two = vec![
            ("a.rs".to_string(), "x".to_string()),
            ("b.rs".to_string(), "y".to_string()),
        ];
        let rev: Vec<_> = two.iter().rev().cloned().collect();
        assert_eq!(whole_object_hash(&two), whole_object_hash(&rev));
    }

    #[test]
    fn whole_input_key_commits_root_parser_and_target() {
        let k = whole_input_key(pid(3), pid(1), "f");
        assert_ne!(k, whole_input_key(pid(4), pid(1), "f"));
        assert_ne!(k, whole_input_key(pid(3), pid(2), "f"));
        assert_ne!(k, whole_input_key(pid(3), pid(1), "g"));
        assert_eq!(k, whole_input_key(pid(3), pid(1), "f"));
    }

    // ── Hand-built substrate: exact closures, exact reviews ─────────────

    fn d(token: &str, source: &str, kind: &str) -> DefRow {
        DefRow {
            token: token.into(),
            source_id: source.into(),
            kind: kind.into(),
        }
    }

    fn r(token: &str, qualifier: Option<&str>, container: &str, source: &str) -> RefRow {
        RefRow {
            token: token.into(),
            qualifier: qualifier.map(Into::into),
            container: Some(container.into()),
            source_id: source.into(),
        }
    }

    fn imp(alias: &str, path: &str, source: &str) -> ImportRow {
        ImportRow {
            alias: alias.into(),
            path: path.into(),
            source_id: source.into(),
        }
    }

    /// `F` (in main.rs) calls `w` and `m::p`. The imports exercise each arm
    /// of the import filter: alias named by a token, alias named by a
    /// qualifier, an alias F names in another file, and an alias F never
    /// names in its own file.
    fn fixture() -> FactSubstrate {
        let defs = vec![
            d("p", "m.rs", "function"),
            d("w", "lib.rs", "function"),
            d("w", "m.rs", "function"),
            d("z", "m.rs", "function"),
        ];
        let refs = vec![
            r("p", Some("m"), "F", "main.rs"),
            r("w", None, "F", "main.rs"),
            r("z", None, "G", "other.rs"),
        ];
        let imports = vec![
            imp("m", "crate::m", "main.rs"),
            imp("w", "crate::lib::w", "main.rs"),
            imp("w", "crate::m::w", "main.rs"),
            imp("w", "crate::x::w", "other.rs"),
            imp("q", "crate::q", "main.rs"),
        ];
        let mut by_fn = BTreeMap::new();
        by_fn.insert("F".to_string(), "F".to_string());
        FactSubstrate::from_rows(defs, refs, imports, by_fn, pid(1))
    }

    #[test]
    fn substrate_accessors() {
        let s = fixture();
        assert_eq!(s.parser_id(), pid(1));
        assert_eq!(s.container_for_fn("F"), Some("F"));
        assert_eq!(s.container_for_fn("nope"), None);
    }

    #[test]
    fn call_target_closure_is_exact() {
        let s = fixture();
        let mut touched = 0;
        let rows = call_target_closure(&s, "F", &mut touched);
        let got: Vec<(RowKind, Provenance)> = rows.iter().map(|r| (r.kind, r.provenance)).collect();
        assert_eq!(
            got,
            vec![
                (RowKind::Container, Provenance::Query),
                (RowKind::Ref, Provenance::Ref(0)),
                (RowKind::Ref, Provenance::Ref(1)),
                (RowKind::Import, Provenance::Import(0)),
                (RowKind::Import, Provenance::Import(1)),
                (RowKind::Import, Provenance::Import(2)),
                (RowKind::Def, Provenance::Def(0)),
                (RowKind::Def, Provenance::Def(1)),
                (RowKind::Def, Provenance::Def(2)),
            ]
        );
        // 2 refs + the 4 imports of main.rs + 3 defs looked up; other.rs's
        // import is never read.
        assert_eq!(touched, 9);
        assert_eq!(rows[1].fields, vec![b"p".to_vec(), vec![1, b'm']]);
        assert_eq!(rows[2].fields, vec![b"w".to_vec(), vec![0]]);
        assert_eq!(
            rows[6].fields,
            vec![b"p".to_vec(), b"m.rs".to_vec(), b"function".to_vec()]
        );
        assert_eq!(rows[3].fields, vec![b"m".to_vec(), b"crate::m".to_vec()]);

        let mut t2 = 0;
        assert_eq!(
            restriction_for_call_target(&s, "F", &mut t2),
            restriction_key(CALL_TARGET_SCHEME, pid(1), &rows)
        );
        assert_eq!(t2, touched);
    }

    #[test]
    fn closure_of_an_unknown_container_is_the_query_row() {
        let s = fixture();
        let mut touched = 0;
        let rows = call_target_closure(&s, "nobody", &mut touched);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::Container);
        assert_eq!(rows[0].fields, vec![b"nobody".to_vec()]);
        assert_eq!(touched, 0, "no refs, so no file's imports are read");
    }

    #[test]
    fn review_call_targets_is_exact() {
        let s = fixture();
        let mut touched = 0;
        let edges: Vec<ResolvedEdge> = review_call_targets(&s, "F", &mut touched)
            .into_iter()
            .collect();
        assert_eq!(
            edges,
            vec![
                ResolvedEdge {
                    token: "p".into(),
                    qualifier: Some("m".into()),
                    via_import: Some("crate::m".into()),
                    candidates: vec![d("p", "m.rs", "function")],
                },
                ResolvedEdge {
                    token: "w".into(),
                    qualifier: None,
                    via_import: Some("crate::lib::w".into()),
                    candidates: vec![d("w", "lib.rs", "function"), d("w", "m.rs", "function")],
                },
            ]
        );
        // per ref: 1 + 5 imports + 4 defs.
        assert_eq!(touched, 20);
    }

    fn api(public: bool) -> ApiDefRow {
        ApiDefRow {
            token: "score".into(),
            qualifier: Some("Widget".into()),
            kind: "function".into(),
            source_id: "main.rs".into(),
            is_public: public,
            sig_identity: [7; 32],
            sig_type_tokens: vec!["w".into(), "i64".into(), "w".into()],
        }
    }

    #[test]
    fn public_api_closure_is_exact() {
        let s = fixture();
        let mut touched = 0;
        let rows = public_api_closure(&s, &api(true), &mut touched);
        let got: Vec<(RowKind, Provenance)> = rows.iter().map(|r| (r.kind, r.provenance)).collect();
        assert_eq!(
            got,
            vec![
                (RowKind::Api, Provenance::Query),
                (RowKind::Qualifier, Provenance::Query),
                (RowKind::Visibility, Provenance::Query),
                (RowKind::Signature, Provenance::Query),
                (RowKind::SigType, Provenance::SigType(0)),
                (RowKind::SigType, Provenance::SigType(1)),
                (RowKind::TypeDef, Provenance::Def(1)),
                (RowKind::TypeDef, Provenance::Def(2)),
            ]
        );
        assert_eq!(touched, 2);
        assert_eq!(
            rows[0].fields,
            vec![b"score".to_vec(), b"function".to_vec()]
        );
        assert_eq!(rows[1].fields, vec![b"\x01Widget".to_vec()]);
        assert_eq!(rows[2].fields, vec![vec![1]]);
        assert_eq!(rows[3].fields, vec![vec![7; 32]]);
        assert_eq!(rows[4].fields, vec![b"i64".to_vec()]);

        let private = public_api_closure(&s, &api(false), &mut 0);
        assert_eq!(private[2].fields, vec![vec![0]]);

        let mut t2 = 0;
        assert_eq!(
            restriction_for_public_api(&s, &api(true), &mut t2),
            restriction_key(PUBLIC_API_SCHEME, pid(1), &rows)
        );
        assert_eq!(t2, touched);
    }

    #[test]
    fn review_public_api_is_exact() {
        let s = fixture();
        let mut touched = 0;
        let surface = review_public_api(&s, &api(true), &mut touched);
        assert_eq!(
            surface,
            PublicSurface {
                exported: true,
                token: "score".into(),
                qualifier: Some("Widget".into()),
                kind: "function".into(),
                resolved_types: vec![
                    ("i64".into(), vec![]),
                    (
                        "w".into(),
                        vec![d("w", "lib.rs", "function"), d("w", "m.rs", "function")]
                    ),
                ],
            }
        );
        assert_eq!(touched, 8, "2 type tokens x 4 defs");

        let mut t2 = 0;
        let hidden = review_public_api(&s, &api(false), &mut t2);
        assert!(!hidden.exported);
        assert!(hidden.resolved_types.is_empty());
        assert_eq!(hidden.token, "score");
        assert_eq!(hidden.qualifier.as_deref(), Some("Widget"));
        assert_eq!(hidden.kind, "function");
        assert_eq!(t2, 0);
    }

    #[test]
    fn stats_counts_false_and_sound_skips() {
        let fx = |changed: bool, skip: bool| FixtureResult {
            name: String::new(),
            review_changed: changed,
            skips: [(Policy::Restriction, skip)].into_iter().collect(),
        };
        let results = [
            fx(true, true),
            fx(false, true),
            fx(false, true),
            fx(true, false),
            fx(false, false),
        ];
        let s = stats(&results, Policy::Restriction);
        assert_eq!((s.false_skips, s.sound_skips), (1, 2));
    }

    // ── Injectivity over row sets (ADR-0032 D2, finding 80f037 F5) ──────

    /// The pre-754aa1 key, kept test-only so the property below can be shown
    /// to FAIL on it: fields joined with U+001F, rows terminated by `\n`, σ of
    /// the string.
    mod legacy {
        use super::super::*;

        pub type AbstractRow = (RowKind, Vec<String>);

        fn name(kind: RowKind) -> &'static str {
            match kind {
                RowKind::Ref => "ref",
                RowKind::Import => "import",
                _ => "def",
            }
        }

        pub fn from_rows(rows: &[Row]) -> Vec<AbstractRow> {
            rows.iter()
                .map(|r| {
                    (
                        r.kind,
                        r.fields
                            .iter()
                            .map(|f| String::from_utf8(f.clone()).unwrap())
                            .collect(),
                    )
                })
                .collect()
        }

        pub fn key(rows: &[AbstractRow]) -> [u8; 32] {
            let mut buf = String::new();
            for (kind, fields) in rows {
                buf.push_str(name(*kind));
                for f in fields {
                    buf.push('\u{1f}');
                    buf.push_str(f);
                }
                buf.push('\n');
            }
            *buf.as_bytes().hash().as_bytes()
        }

        /// One row whose last field swallows a second row. Under the legacy
        /// framing `[a, b]` and `[merge(a, b)]` serialize identically.
        pub fn merge(a: &AbstractRow, b: &AbstractRow) -> AbstractRow {
            let mut out = a.clone();
            let mut tail = format!("\n{}", name(b.0));
            for f in &b.1 {
                tail.push('\u{1f}');
                tail.push_str(f);
            }
            match out.1.last_mut() {
                Some(last) => last.push_str(&tail),
                None => out.1.push(tail),
            }
            out
        }
    }

    mod injectivity {
        use super::legacy::{self, AbstractRow};
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

        /// Fields over an alphabet built to break separator framing: U+001F,
        /// newline, '/', and a shared `src/` prefix.
        fn field() -> impl Strategy<Value = String> {
            (
                any::<bool>(),
                prop::collection::vec(
                    prop::sample::select(vec!['a', 'b', '/', '\n', '\u{1f}']),
                    0..4,
                ),
            )
                .prop_map(|(prefixed, cs)| {
                    let body: String = cs.into_iter().collect();
                    if prefixed {
                        format!("src/{body}")
                    } else {
                        body
                    }
                })
        }

        fn abstract_row() -> impl Strategy<Value = AbstractRow> {
            prop_oneof![
                prop::collection::vec(field(), 2).prop_map(|f| (RowKind::Ref, f)),
                prop::collection::vec(field(), 2).prop_map(|f| (RowKind::Import, f)),
                prop::collection::vec(field(), 3).prop_map(|f| (RowKind::Def, f)),
            ]
        }

        /// Pairs of row sets: independent ones, and pairs where one row
        /// swallows another under the legacy framing.
        fn pair() -> impl Strategy<Value = (Vec<AbstractRow>, Vec<AbstractRow>)> {
            let rows = || prop::collection::vec(abstract_row(), 0..4);
            prop_oneof![
                (rows(), rows()),
                (abstract_row(), abstract_row(), rows()).prop_map(|(a, b, rest)| {
                    let mut left = rest.clone();
                    left.push(a.clone());
                    left.push(b.clone());
                    let mut right = rest;
                    right.push(legacy::merge(&a, &b));
                    (left, right)
                }),
            ]
        }

        fn to_rows(rows: &[AbstractRow]) -> Vec<Row> {
            rows.iter()
                .map(|(kind, fields)| Row {
                    kind: *kind,
                    fields: fields.iter().map(|f| f.as_bytes().to_vec()).collect(),
                    provenance: Provenance::Query,
                })
                .collect()
        }

        fn multiset(rows: &[AbstractRow]) -> Vec<AbstractRow> {
            let mut v = rows.to_vec();
            v.sort();
            v
        }

        fn new_key(rows: &[AbstractRow]) -> Hash {
            restriction_key(CALL_TARGET_SCHEME, Hash::ZERO, &to_rows(rows))
        }

        proptest! {
            #![proptest_config(Config { cases: 1024, ..Config::default() })]

            /// Distinct row multisets give distinct keys.
            #[test]
            fn distinct_row_sets_give_distinct_keys((a, b) in pair()) {
                prop_assume!(multiset(&a) != multiset(&b));
                prop_assert_ne!(new_key(&a), new_key(&b));
            }

            /// Equal row multisets give equal keys, whatever the order.
            #[test]
            fn equal_row_sets_give_equal_keys(a in prop::collection::vec(abstract_row(), 0..5)) {
                let mut rev = a.clone();
                rev.reverse();
                prop_assert_eq!(new_key(&a), new_key(&rev));
            }
        }

        /// The same property, run on the legacy key, is falsified. Seeded so
        /// the counterexample search is deterministic.
        #[test]
        fn the_legacy_key_falsifies_injectivity() {
            let mut runner = TestRunner::new_with_rng(
                Config {
                    cases: 1024,
                    failure_persistence: None,
                    ..Config::default()
                },
                TestRng::deterministic_rng(RngAlgorithm::ChaCha),
            );
            let outcome = runner.run(&pair(), |(a, b)| {
                if multiset(&a) != multiset(&b) {
                    prop_assert_ne!(legacy::key(&a), legacy::key(&b));
                }
                Ok(())
            });
            assert!(
                outcome.is_err(),
                "the separator-join key was expected to collide on some pair"
            );
        }
    }
}
