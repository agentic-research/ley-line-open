//! Generic `.scm`-driven extraction engine.
//!
//! Replaces per-language hand-written tree walks (`extract_<lang>` in
//! `refs.rs`) with one interpreter over tree-sitter query files. The
//! per-language knowledge lives in `queries/<lang>/tags.scm` — data,
//! not code. Bead `ley-line-open-206d53`.
//!
//! # Emission vocabulary (the query→fact ABI)
//!
//! A pattern's ROOT capture names the fact kind; inner captures name
//! its fields:
//!
//! - `@def` on the pattern root → [`ExtractedRef::Def`] anchored at
//!   that node (`canonical_kind` derived from the anchor's raw kind
//!   via [`TsLanguage::canonical_kind`])
//! - `@ref` on the pattern root → [`ExtractedRef::Ref`]
//! - `@import` on the pattern root → [`ExtractedRef::Import`]
//! - `@name` → the emitted token (empty text suppresses the emission)
//! - `@qualifier` → when captured, emit `{qualifier}{sep}{name}` FIRST,
//!   then the bare `{name}` (dual-emit: consumers join on the
//!   qualified form, call-side resolution uses the bare form). `sep`
//!   defaults to `.`; a pattern overrides it with
//!   `(#set! qualifier-separator "::")` — Rust fixtures pin
//!   `std::process::exit`-shaped tokens, so the separator is
//!   per-pattern data, not engine code. On Ref pairs the BARE row also
//!   carries the qualifier text structurally
//!   ([`ExtractedRef::Ref`]`::qualifier` → `node_refs.qualifier`, bead
//!   `ley-line-open-4dde42`); the qualified row's field stays `None`
//!   (its token embeds the qualifier)
//! - `@path` → import path; surrounding delimiters are stripped
//!   (string-literal quotes, and the `<`/`>` of a C/C++
//!   `system_lib_string` — `#include <stdio.h>` carries the brackets
//!   in the node text)
//! - `@alias` → import alias; missing, empty, or `.` defaults to the
//!   path's last `/` segment
//!
//! The engine is invoked per named node during the content-addressing
//! fold (`extract_refs` dispatch), so matching is anchored: only
//! patterns whose root IS the given node emit. This preserves the
//! per-node `node_id` and `container_node_id` threading the fold
//! already does — no byte-range→node_id index is needed.
//!
//! # Arena-resident overrides (bead `ley-line-open-e72629`)
//!
//! An arena may carry OVERRIDE `.scm` blobs that replace the compiled-in
//! `tags.scm` for a language, behind a BLAKE3-hash allowlist
//! (operator-controlled `LLO_TRUSTED_QUERY_HASHES`). [`resolve_query_set`]
//! turns an arena's `_queries`/`query_blobs` rows into a [`QuerySet`];
//! the fold calls [`crate::refs::extract_refs_resolved`], which uses a
//! trusted override engine when present, else the compiled default.
//!
//! An override REPLACES the compiled `.scm`-driven emission for its
//! language. The query-inexpressible imperative arms in `refs.rs`
//! (use-list flattening, from-import joins, qualified `Class.method`
//! defs, JS/TS κ fixups) do NOT re-run under an override — an operator
//! shipping an override owns the complete emission for that language via
//! the `.scm`. This is a lossless swap for the pure-delegate languages
//! (Go / C / SQL / Bash) and an explicit trade for the rest.
//!
//! ## Emission-ABI declaration
//!
//! An override blob MUST declare the emission vocabulary it was written
//! against with a leading comment:
//!
//! ```scheme
//! ; emission-abi-version: 1
//! ```
//!
//! [`QueryEngine::new_override`] hard-errors at load when the
//! declaration is missing, when its version disagrees with
//! [`EMISSION_ABI_VERSION`], or when the blob uses a capture name
//! outside the emission vocabulary above (`_`-prefixed predicate
//! helpers excepted). A malformed TRUSTED override is an operator error
//! that fails loud — never a silent no-op. Untrusted blobs are ignored
//! before the gate (one stderr line, compiled fallback).
//!
//! ## Resource bounds
//!
//! Override engines are the untrusted-input surface, so their per-node
//! runs ([`QueryEngine::extract_bounded`]) carry a match-state ceiling
//! and a progress-callback tick budget. A pathological blob degrades to
//! "no facts for this file" plus one stderr line — never a hung parse.

use std::cell::Cell;
use std::collections::HashSet;
use std::ops::ControlFlow;

use tree_sitter::{
    Node, Query, QueryCursor, QueryCursorOptions, QueryCursorState, QueryMatch, StreamingIterator,
};

use crate::languages::TsLanguage;
use crate::refs::ExtractedRef;

/// Emission-ABI version an arena OVERRIDE `.scm` must declare (via a
/// leading `; emission-abi-version: N` comment). The number pins the
/// (capture-name → fact) vocabulary documented in this file's header;
/// bump it whenever that contract changes in a way that would silently
/// misread an override written against an older vocabulary. Compiled-in
/// defaults are implicitly at the current version and skip the check.
pub const EMISSION_ABI_VERSION: u32 = 1;

/// Capture names the engine turns into facts. An override blob naming
/// any OTHER capture (excluding tree-sitter `_`-prefixed predicate
/// helpers such as bash's `@_cmd`) declares a vocabulary the engine
/// can't honor — a hard load error, never a silent no-op (bead
/// `ley-line-open-e72629`, requirement 3).
const EMISSION_CAPTURES: &[&str] = &["def", "ref", "import", "name", "qualifier", "path", "alias"];

/// Per-node in-progress match-state ceiling for an OVERRIDE engine — the
/// untrusted-input surface. tree-sitter caps this at 65536; the default
/// is far above any legitimate anchored per-node match, so a legitimate
/// override never trips it while a state-explosion pattern does.
/// `LLO_QUERY_MATCH_LIMIT` tunes it (operator knob + test seam).
fn override_match_limit() -> u32 {
    std::env::var("LLO_QUERY_MATCH_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0 && n <= 65536)
        .unwrap_or(4096)
}

/// Progress-callback tick budget per OVERRIDE-engine node run: a
/// wall-clock-independent ceiling on matcher work so a catastrophic
/// pattern aborts instead of hanging. `LLO_QUERY_PROGRESS_BUDGET` tunes
/// it (operator knob + test seam).
fn override_progress_budget() -> u64 {
    std::env::var("LLO_QUERY_PROGRESS_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(100_000)
}

/// An override engine hit its resource bounds on a node — the caller
/// drops this file's extracted facts (no facts for this file), emits
/// one stderr line, and the parse completes. Never a hang, never a
/// partial fact set.
#[derive(Debug)]
pub struct BoundsExceeded;

/// A compiled query + resolved capture indexes for one language.
pub struct QueryEngine {
    query: Query,
    ts_lang: TsLanguage,
    cap_def: Option<u32>,
    cap_ref: Option<u32>,
    cap_import: Option<u32>,
    cap_name: Option<u32>,
    cap_qualifier: Option<u32>,
    cap_path: Option<u32>,
    cap_alias: Option<u32>,
    /// True for an arena OVERRIDE engine: its runs carry the
    /// match-limit + progress-budget guards. Compiled-in engines are
    /// trusted (fixture-pinned) and run unbounded.
    bounded: bool,
}

impl QueryEngine {
    /// Compile `scm` against the language's grammar. Errors carry the
    /// query-source offset tree-sitter reports, so a broken pattern in
    /// a compiled-in `.scm` fails loudly at first use, not silently.
    pub fn new(ts_lang: TsLanguage, scm: &str) -> anyhow::Result<Self> {
        let language = ts_lang.ts_language();
        let query = Query::new(&language, scm)
            .map_err(|e| anyhow::anyhow!("tags.scm for {}: {e}", ts_lang.name()))?;
        let cap = |name: &str| query.capture_index_for_name(name);
        Ok(Self {
            cap_def: cap("def"),
            cap_ref: cap("ref"),
            cap_import: cap("import"),
            cap_name: cap("name"),
            cap_qualifier: cap("qualifier"),
            cap_path: cap("path"),
            cap_alias: cap("alias"),
            query,
            ts_lang,
            bounded: false,
        })
    }

    /// Compile a TRUSTED arena override `.scm` behind the emission-ABI
    /// gate (bead `ley-line-open-e72629`). Two hard-error conditions,
    /// distinct from compiled-in construction because the operator owns
    /// this blob's correctness:
    ///
    /// 1. The blob must declare `; emission-abi-version: N` and `N` must
    ///    equal [`EMISSION_ABI_VERSION`] — a missing or mismatched
    ///    declaration means the blob speaks a vocabulary the engine
    ///    can't be sure it reads correctly.
    /// 2. Every capture name must be in [`EMISSION_CAPTURES`] or be a
    ///    `_`-prefixed predicate helper — a capture outside the emission
    ///    vocabulary is a fact the engine has no rule for.
    ///
    /// The resulting engine carries the override resource bounds (see
    /// [`QueryEngine::extract_bounded`]).
    pub fn new_override(ts_lang: TsLanguage, scm: &str) -> anyhow::Result<Self> {
        let declared = parse_emission_abi_version(scm).ok_or_else(|| {
            anyhow::anyhow!(
                "override .scm for {} must declare `; emission-abi-version: N`",
                ts_lang.name()
            )
        })?;
        if declared != EMISSION_ABI_VERSION {
            anyhow::bail!(
                "override .scm for {} declares emission-abi-version {declared}, \
                 engine speaks {EMISSION_ABI_VERSION}",
                ts_lang.name()
            );
        }
        let mut engine = Self::new(ts_lang, scm)?;
        for name in engine.query.capture_names() {
            if name.starts_with('_') || EMISSION_CAPTURES.contains(name) {
                continue;
            }
            anyhow::bail!(
                "override .scm for {} uses capture @{name} outside the emission \
                 vocabulary {EMISSION_CAPTURES:?}",
                ts_lang.name()
            );
        }
        engine.bounded = true;
        Ok(engine)
    }

    /// Emit facts for patterns anchored exactly at `node`.
    ///
    /// Same contract as the hand-written extractors this replaces:
    /// pure data, no DB access, safe for parallel use.
    pub fn extract(
        &self,
        node: &Node,
        source: &[u8],
        node_id: &str,
        source_id: &str,
        container_node_id: Option<&str>,
    ) -> Vec<ExtractedRef> {
        let mut out = Vec::new();
        let mut cursor = QueryCursor::new();
        // Only match patterns whose root is `node` itself — the fold
        // visits every named node, so unanchored matching would emit
        // each fact once per ancestor.
        cursor.set_max_start_depth(Some(0));
        let mut matches = cursor.matches(&self.query, *node, source);
        while let Some(m) = matches.next() {
            self.push_match(
                m,
                node,
                source,
                node_id,
                source_id,
                container_node_id,
                &mut out,
            );
        }
        out
    }

    /// [`QueryEngine::extract`] under the OVERRIDE resource bounds
    /// (bead `ley-line-open-e72629`, requirement 4). Runs with a
    /// per-node in-progress match ceiling ([`override_match_limit`]) and
    /// a progress-callback tick budget ([`override_progress_budget`]);
    /// tripping either returns [`BoundsExceeded`] so a pathological blob
    /// degrades to "no facts for this file" plus one stderr line, never
    /// a hung parse. Only override engines take this path — compiled-in
    /// engines are fixture-pinned and run [`QueryEngine::extract`].
    pub fn extract_bounded(
        &self,
        node: &Node,
        source: &[u8],
        node_id: &str,
        source_id: &str,
        container_node_id: Option<&str>,
    ) -> Result<Vec<ExtractedRef>, BoundsExceeded> {
        let mut out = Vec::new();
        let mut cursor = QueryCursor::new();
        cursor.set_max_start_depth(Some(0));
        cursor.set_match_limit(override_match_limit());
        let budget = override_progress_budget();
        let ticks = Cell::new(0u64);
        {
            let mut progress = |_: &QueryCursorState| -> ControlFlow<()> {
                let t = ticks.get() + 1;
                ticks.set(t);
                if t > budget {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let opts = QueryCursorOptions::new().progress_callback(&mut progress);
            let mut matches = cursor.matches_with_options(&self.query, *node, source, opts);
            while let Some(m) = matches.next() {
                self.push_match(
                    m,
                    node,
                    source,
                    node_id,
                    source_id,
                    container_node_id,
                    &mut out,
                );
            }
        }
        if cursor.did_exceed_match_limit() || ticks.get() > budget {
            return Err(BoundsExceeded);
        }
        Ok(out)
    }

    /// Turn one anchored [`QueryMatch`] into fact rows. Shared by the
    /// unbounded ([`QueryEngine::extract`]) and bounded
    /// ([`QueryEngine::extract_bounded`]) paths so both interpret the
    /// emission vocabulary identically.
    #[allow(clippy::too_many_arguments)]
    fn push_match(
        &self,
        m: &QueryMatch,
        node: &Node,
        source: &[u8],
        node_id: &str,
        source_id: &str,
        container_node_id: Option<&str>,
        out: &mut Vec<ExtractedRef>,
    ) {
        let text = |idx: Option<u32>| -> Option<&str> {
            let idx = idx?;
            m.captures
                .iter()
                .find(|c| c.index == idx)
                .and_then(|c| c.node.utf8_text(source).ok())
        };
        let anchored = |idx: Option<u32>| -> bool {
            idx.is_some_and(|idx| {
                m.captures
                    .iter()
                    .any(|c| c.index == idx && c.node.id() == node.id())
            })
        };

        if anchored(self.cap_import) {
            let Some(path) = text(self.cap_path) else {
                return;
            };
            // Delimiter stripping is generic engine behavior, not
            // language data: quotes/backticks wrap string-literal
            // paths (Go, JS/TS), `<`/`>` wrap a C/C++
            // system_lib_string (`#include <stdio.h>` — bead
            // ley-line-open-5e21c2). No language's import path
            // legitimately starts or ends with any of these.
            let path = path.trim_matches(|c| matches!(c, '"' | '`' | '<' | '>'));
            if path.is_empty() {
                return;
            }
            let alias = text(self.cap_alias).unwrap_or("");
            let alias = if alias.is_empty() || alias == "." {
                path.rsplit('/').next().unwrap_or(path)
            } else {
                alias
            };
            out.push(ExtractedRef::Import {
                alias: alias.to_string(),
                path: path.to_string(),
                source_id: source_id.to_string(),
            });
            return;
        }

        let is_def = anchored(self.cap_def);
        let is_ref = anchored(self.cap_ref);
        if !is_def && !is_ref {
            return;
        }
        let Some(name) = text(self.cap_name) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        let canonical_kind = self.ts_lang.canonical_kind(node.kind());
        // `qualifier` rides only on Ref rows (bead
        // `ley-line-open-4dde42`): a def's qualified form stays a
        // token-only dual-emit — node_defs has no qualifier column.
        let mut push = |token: String, qualifier: Option<String>| {
            if is_def {
                out.push(ExtractedRef::Def {
                    token,
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind,
                });
            } else {
                out.push(ExtractedRef::Ref {
                    token,
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    qualifier,
                });
            }
        };
        if let Some(qualifier) = text(self.cap_qualifier)
            && !qualifier.is_empty()
        {
            // Per-pattern `(#set! qualifier-separator "::")`
            // overrides the `.` default — the separator is language
            // data (Go pins `pkg.Func`, Rust pins `mod::func`).
            let sep = self
                .query
                .property_settings(m.pattern_index)
                .iter()
                .find(|p| &*p.key == "qualifier-separator")
                .and_then(|p| p.value.as_deref())
                .unwrap_or(".");
            // Qualified row: the token embeds the qualifier, so the
            // structural field stays NULL — exactly ONE row per
            // qualified call site carries the (name, qualifier)
            // pair, and GROUP BY/filter consumers never double-count.
            push(format!("{qualifier}{sep}{name}"), None);
            push(name.to_string(), Some(qualifier.to_string()));
        } else {
            push(name.to_string(), None);
        }
    }
}

// ---------------------------------------------------------------------------
// Arena-resident override resolution (bead ley-line-open-e72629)
// ---------------------------------------------------------------------------

/// Parse the `; emission-abi-version: N` declaration from an override
/// blob. Scans comment lines (tree-sitter `.scm` comments start with
/// `;`); returns the first declared version, or `None` when absent.
/// Accepts either spelling (`emission-abi-version` / `emission_abi_version`).
fn parse_emission_abi_version(scm: &str) -> Option<u32> {
    for line in scm.lines() {
        let Some(rest) = line.trim_start().strip_prefix(';') else {
            continue;
        };
        let rest = rest.trim();
        for key in ["emission-abi-version", "emission_abi_version"] {
            if let Some(after) = rest.strip_prefix(key) {
                let after = after.trim_start_matches([':', ' ', '\t']);
                if let Ok(n) = after.trim().parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Length-prefixed append to a σ-epoch preimage (LEB128, same encoding
/// as the merkle-AST fold + the injection epoch) so adjacent free-form
/// `.scm`/name parts stay unambiguous.
fn push_part(buf: &mut Vec<u8>, part: &[u8]) {
    let mut v = part.len() as u64;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(part);
}

/// A trusted, ABI-gated override engine for one language, plus its
/// BLAKE3 hex (provenance + epoch input).
struct LangOverride {
    lang: TsLanguage,
    engine: QueryEngine,
    hex: String,
}

/// The effective query set for one parse pass: the compiled-in defaults
/// plus any TRUSTED arena overrides. Built once per parse and threaded
/// through the fold by reference — no process-global override state, so
/// per-arena resolution stays correct under the daemon (which parses
/// many arenas in one process).
pub struct QuerySet {
    overrides: Vec<LangOverride>,
}

impl QuerySet {
    /// The all-compiled query set (no overrides). Used for the in-memory
    /// `parse_to_ast_json` path and anywhere without an arena.
    pub fn compiled() -> Self {
        Self {
            overrides: Vec::new(),
        }
    }

    /// The trusted override engine for `lang`, or `None` when the
    /// language falls back to its compiled-in default.
    pub fn override_engine(&self, lang: TsLanguage) -> Option<&QueryEngine> {
        self.overrides
            .iter()
            .find(|o| o.lang == lang)
            .map(|o| &o.engine)
    }

    /// Active `(lang, blake3-hex)` overrides, sorted by language name —
    /// the provenance surface (`_meta.query_source:<lang>`) and the
    /// [`query_set_epoch`] input.
    pub fn active(&self) -> Vec<(TsLanguage, String)> {
        let mut v: Vec<(TsLanguage, String)> = self
            .overrides
            .iter()
            .map(|o| (o.lang, o.hex.clone()))
            .collect();
        v.sort_by(|a, b| a.0.name().cmp(b.0.name()));
        v
    }

    /// True when every language uses its compiled-in default.
    pub fn is_all_compiled(&self) -> bool {
        self.overrides.is_empty()
    }
}

/// Outcome of resolving an arena's overrides: the effective [`QuerySet`]
/// plus the operator-facing warnings (one per IGNORED override). The
/// caller prints one stderr line per warning — an untrusted/corrupt
/// blob never silently changes extraction, and never silently drops to
/// no extraction (it falls back to the compiled default).
pub struct QueryResolution {
    pub query_set: QuerySet,
    pub warnings: Vec<String>,
}

/// Resolve the effective query set for `conn`'s arena against the
/// operator `allowlist` (lowercase BLAKE3-hex) (bead
/// `ley-line-open-e72629`). Per `kind='tags'` override row:
///
/// - blob bytes must hash to the pointer's stored key (integrity) — a
///   mismatch is treated as untrusted;
/// - the hash must be in `allowlist` — an unknown hash is IGNORED with
///   one warning and the compiled default is used;
/// - a TRUSTED blob then passes the emission-ABI gate
///   ([`QueryEngine::new_override`]) or the whole resolve fails loud (a
///   trusted-but-malformed blob is an operator error, not a silent
///   no-op).
///
/// Compiled-in defaults are implicitly trusted and never appear here.
pub fn resolve_query_set(
    conn: &rusqlite::Connection,
    allowlist: &HashSet<String>,
) -> anyhow::Result<QueryResolution> {
    use leyline_core::ContentAddressed;

    let rows = crate::schema::read_query_overrides(conn)?;
    let mut overrides = Vec::new();
    let mut warnings = Vec::new();
    for row in rows {
        let Ok(lang) = TsLanguage::from_name(&row.lang) else {
            warnings.push(format!(
                "query override for unknown language '{}' ignored",
                row.lang
            ));
            continue;
        };
        let h = row.blob_bytes.as_slice().hash();
        if h.as_bytes()[..] != row.blob_hash[..] {
            warnings.push(format!(
                "query override for {} has a blob-hash mismatch (corrupt or tampered arena row); \
                 ignored, using compiled default",
                lang.name()
            ));
            continue;
        }
        let hex = h.to_string();
        if !allowlist.contains(&hex) {
            warnings.push(format!(
                "query override for {} (blake3 {hex}) is not in LLO_TRUSTED_QUERY_HASHES; \
                 ignored, using compiled default",
                lang.name()
            ));
            continue;
        }
        let scm = std::str::from_utf8(&row.blob_bytes).map_err(|_| {
            anyhow::anyhow!("trusted query override for {} is not UTF-8", lang.name())
        })?;
        let engine = QueryEngine::new_override(lang, scm)?;
        overrides.push(LangOverride { lang, engine, hex });
    }
    Ok(QueryResolution {
        query_set: QuerySet { overrides },
        warnings,
    })
}

/// σ epoch over the ACTIVE override set — the extraction facts an arena
/// built under a given query set must be re-derived when that set
/// changes (bead `ley-line-open-e72629`, requirement 1). Covers the
/// active override BYTES via their BLAKE3 hex (the hex IS the blob's
/// content address), so swapping, adding, or removing an override
/// changes the epoch. Compiled-in `.scm` edits stay covered by the
/// scalar `EXTRACTION_EPOCH` (its manual bump); this dimension is the
/// per-arena override set only. The all-compiled set yields a stable
/// constant.
pub fn query_set_epoch(query_set: &QuerySet) -> String {
    use leyline_core::ContentAddressed;

    let mut p: Vec<u8> = Vec::new();
    p.extend_from_slice(b"llo/query-set-epoch/v1");
    p.push(0x00);
    for (lang, hex) in query_set.active() {
        push_part(&mut p, lang.name().as_bytes());
        push_part(&mut p, hex.as_bytes());
    }
    p.hash().to_string()
}
