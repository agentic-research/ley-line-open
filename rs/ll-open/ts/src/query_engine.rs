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
//! - `@qualifier` → when captured, emit `{qualifier}.{name}` FIRST,
//!   then the bare `{name}` (dual-emit: consumers join on the
//!   qualified form, call-side resolution uses the bare form)
//! - `@path` → import path; surrounding string-literal quotes are
//!   stripped
//! - `@alias` → import alias; missing, empty, or `.` defaults to the
//!   path's last `/` segment
//!
//! The engine is invoked per named node during the content-addressing
//! fold (`extract_refs` dispatch), so matching is anchored: only
//! patterns whose root IS the given node emit. This preserves the
//! per-node `node_id` and `container_node_id` threading the fold
//! already does — no byte-range→node_id index is needed.

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::languages::TsLanguage;
use crate::refs::ExtractedRef;

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
        })
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
                    continue;
                };
                let path = path.trim_matches(|c| c == '"' || c == '`');
                if path.is_empty() {
                    continue;
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
                continue;
            }

            let is_def = anchored(self.cap_def);
            let is_ref = anchored(self.cap_ref);
            if !is_def && !is_ref {
                continue;
            }
            let Some(name) = text(self.cap_name) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let canonical_kind = self.ts_lang.canonical_kind(node.kind());
            let mut push = |token: String| {
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
                    });
                }
            };
            if let Some(qualifier) = text(self.cap_qualifier)
                && !qualifier.is_empty()
            {
                push(format!("{qualifier}.{name}"));
            }
            push(name.to_string());
        }
        out
    }
}
