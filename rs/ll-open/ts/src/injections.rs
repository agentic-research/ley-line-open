//! Embedded-language injection detection + the composite injection
//! epoch (bead `ley-line-open-c822a6`, EXP2 of the queries-as-data
//! design on bead `ley-line-open-e5addb`).
//!
//! # Detection
//!
//! Per host language, an [`InjectionEngine`] compiles the compiled-in
//! `queries/<lang>/injections.scm` (upstream tree-sitter conventions:
//! `@injection.content` capture + `(#set! injection.language "...")`
//! per-pattern property). [`InjectionEngine::sites`] is anchored
//! exactly like `QueryEngine::extract` — only patterns whose ROOT is
//! the given node match — so the content-addressing fold can probe
//! every named node without emitting a site once per ancestor.
//!
//! The reparse-and-extract side lives in cli-lib's `cmd_parse.rs`
//! (`fold_injected`): the captured byte range is reparsed under the
//! target grammar via `Parser::set_included_ranges` and the target
//! language's `tags.scm` runs over the injected tree. The injected
//! subtree gets its OWN content-addressed root — its hashes never
//! enter host preimages, so the host file's structural identity is
//! independent of the injected grammar's version.
//!
//! # Composite injection epoch
//!
//! Injected facts depend on inputs the scalar
//! [`EXTRACTION_EPOCH`](crate::refs::EXTRACTION_EPOCH) does not see:
//! the host's `injections.scm`, the injected language's `tags.scm`,
//! and both grammars. [`current_injection_epoch`] hashes all of them
//! into one σ composite; the parse layer stores it in
//! `_meta.injection_epoch` and ANDs it with `extraction_epoch` in the
//! unchanged-skip gate (cli-lib `parse_into_conn`). Gate test:
//! cli-lib's `f7_injection_epoch_invalidation`.

use std::sync::OnceLock;

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::languages::TsLanguage;

/// Compiled-in Go injections query. Also the default `injections.scm`
/// bytes in the composite epoch preimage.
#[cfg(all(feature = "go", feature = "sql"))]
const GO_INJECTIONS_SCM: &str = include_str!("../queries/go/injections.scm");

/// One embedded-language region: reparse `range` under `language`.
pub struct InjectionSite {
    /// Target (injected) language, resolved from the pattern's
    /// `injection.language` property via [`TsLanguage::from_name`].
    pub language: TsLanguage,
    /// Byte/point range of the `@injection.content` capture in the
    /// HOST source. `Parser::set_included_ranges` keeps host offsets,
    /// so injected-tree spans remain host-file positions.
    pub range: tree_sitter::Range,
}

/// A compiled `injections.scm` for one host language.
pub struct InjectionEngine {
    query: Query,
    cap_content: u32,
}

impl InjectionEngine {
    /// Compile `scm` against the host grammar. Same fail-loud contract
    /// as `QueryEngine::new`: a broken compiled-in query errors at
    /// first use with tree-sitter's source offset.
    fn new(host: TsLanguage, scm: &str) -> anyhow::Result<Self> {
        let query = Query::new(&host.ts_language(), scm)
            .map_err(|e| anyhow::anyhow!("injections.scm for {}: {e}", host.name()))?;
        let cap_content = query
            .capture_index_for_name("injection.content")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "injections.scm for {} has no @injection.content capture",
                    host.name()
                )
            })?;
        Ok(Self { query, cap_content })
    }

    /// Injection sites for patterns anchored exactly at `node`.
    ///
    /// Sites whose `injection.language` doesn't resolve to a
    /// compiled-in [`TsLanguage`] (feature off) and empty content
    /// ranges are skipped — detection degrades to "no injection",
    /// never to an error. Pure data, no DB access, safe for parallel
    /// use (mirrors `QueryEngine::extract`).
    pub fn sites(&self, node: &Node, source: &[u8]) -> Vec<InjectionSite> {
        let mut out = Vec::new();
        let mut cursor = QueryCursor::new();
        // Anchored: the fold visits every named node, so unanchored
        // matching would emit each site once per ancestor.
        cursor.set_max_start_depth(Some(0));
        let mut matches = cursor.matches(&self.query, *node, source);
        while let Some(m) = matches.next() {
            let Some(language) = self
                .query
                .property_settings(m.pattern_index)
                .iter()
                .find(|p| &*p.key == "injection.language")
                .and_then(|p| p.value.as_deref())
                .and_then(|name| TsLanguage::from_name(name).ok())
            else {
                continue;
            };
            for c in m.captures.iter().filter(|c| c.index == self.cap_content) {
                if c.node.end_byte() > c.node.start_byte() {
                    out.push(InjectionSite {
                        language,
                        range: c.node.range(),
                    });
                }
            }
        }
        out
    }
}

/// The injection engine for `host`, or `None` when the host language
/// ships no `injections.scm` (every language but Go today) or the
/// target grammar's feature is off. Compiled once per process.
pub fn injection_engine(host: TsLanguage) -> Option<&'static InjectionEngine> {
    match host {
        #[cfg(all(feature = "go", feature = "sql"))]
        TsLanguage::Go => {
            static ENGINE: OnceLock<InjectionEngine> = OnceLock::new();
            Some(ENGINE.get_or_init(|| {
                InjectionEngine::new(TsLanguage::Go, GO_INJECTIONS_SCM).expect(
                    "compiled-in queries/go/injections.scm must compile against tree-sitter-go",
                )
            }))
        }
        _ => None,
    }
}

/// True when `LLO_DISABLE_INJECTIONS=1`. Falsification seam for the
/// host-hash-independence gate (cli-lib's
/// `inj_host_node_hashes_independent_of_injection_pass`), NOT a user
/// knob: it does not enter [`current_injection_epoch`], so toggling it
/// against a live arena leaves facts stale by design — tests own both
/// sides of the toggle.
pub fn injections_disabled() -> bool {
    std::env::var("LLO_DISABLE_INJECTIONS").ok().as_deref() == Some("1")
}

/// Append a length-prefixed part to the composite preimage. LEB128
/// length prefix (same encoding as the merkle-AST fold's
/// `write_uvarint`) keeps adjacent parts unambiguous — `.scm` bytes
/// are free-form text.
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

/// In-process grammar fingerprint: ABI version + node-kind count +
/// field count. A proxy for the grammar crate's version (crates expose
/// no version API at the `Language` level): virtually every grammar
/// release changes the kind or field inventory. A release that changes
/// parse SHAPES without touching either inventory is invisible here —
/// that case still requires a manual [`crate::refs::EXTRACTION_EPOCH`]
/// bump, same as any other emission-affecting change.
fn grammar_fingerprint(lang: TsLanguage) -> String {
    let l = lang.ts_language();
    format!(
        "{}:{}:{}:{}",
        lang.name(),
        l.abi_version(),
        l.node_kind_count(),
        l.field_count()
    )
}

/// Composite injection epoch: hex σ hash over everything injected-fact
/// derivation depends on beyond the source bytes —
///
/// - [`current_extraction_epoch`](crate::refs::current_extraction_epoch)
///   (the scalar emission-rules epoch),
/// - per (host → target) injection pair: both grammar fingerprints,
///   the host's `injections.scm` bytes, and the target's `tags.scm`
///   bytes.
///
/// Stored in `_meta.injection_epoch` by the parse layer and compared
/// on arena adoption; any disagreement — including the missing row
/// every pre-injection arena has — forces full fact re-derivation.
/// That missing-row staleness is also what delivers injected facts to
/// existing arenas on upgrade, which is why shipping injections needs
/// no `EXTRACTION_EPOCH` bump.
///
/// `LLO_INJECTIONS_SCM` substitutes the `injections.scm` bytes in the
/// preimage — the f7 test seam, same convention as
/// `LLO_EXTRACTION_EPOCH` (changes the epoch input, not the emission).
pub fn current_injection_epoch() -> String {
    use leyline_core::ContentAddressed;

    let mut p: Vec<u8> = Vec::new();
    p.extend_from_slice(b"llo/injection-epoch/v1");
    p.push(0x00);
    push_part(
        &mut p,
        crate::refs::current_extraction_epoch()
            .to_string()
            .as_bytes(),
    );

    #[cfg(all(feature = "go", feature = "sql"))]
    {
        let scm_override = std::env::var("LLO_INJECTIONS_SCM").ok();
        let injections_scm = scm_override.as_deref().unwrap_or(GO_INJECTIONS_SCM);
        push_part(&mut p, grammar_fingerprint(TsLanguage::Go).as_bytes());
        push_part(&mut p, injections_scm.as_bytes());
        push_part(&mut p, grammar_fingerprint(TsLanguage::Sql).as_bytes());
        push_part(&mut p, include_str!("../queries/sql/tags.scm").as_bytes());
    }

    p.hash().to_string()
}

#[cfg(test)]
#[cfg(all(feature = "go", feature = "sql"))]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_go(src: &[u8]) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser.set_language(&TsLanguage::Go.ts_language()).unwrap();
        parser.parse(src, None).unwrap()
    }

    /// Walk every named node the way the fold does and collect the
    /// (language, content-text) of each detected site.
    fn detect_all(src: &[u8]) -> Vec<(&'static str, String)> {
        let tree = parse_go(src);
        let engine = injection_engine(TsLanguage::Go).expect("go ships injections.scm");
        let mut out = Vec::new();
        fn walk(
            node: tree_sitter::Node,
            src: &[u8],
            engine: &InjectionEngine,
            out: &mut Vec<(&'static str, String)>,
        ) {
            for site in engine.sites(&node, src) {
                let text = std::str::from_utf8(&src[site.range.start_byte..site.range.end_byte])
                    .unwrap()
                    .to_string();
                out.push((site.language.name(), text));
            }
            let mut c = node.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().is_named() {
                        walk(c.node(), src, engine, out);
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), src, engine, &mut out);
        out
    }

    #[test]
    fn detects_sql_in_raw_and_interpreted_strings() {
        let src = br#"package main

func f() {
	a := `CREATE TABLE t (id INTEGER)`
	b := "SELECT name FROM users"
	_, _ = a, b
}
"#;
        let sites = detect_all(src);
        assert_eq!(
            sites,
            vec![
                ("sql", "CREATE TABLE t (id INTEGER)".to_string()),
                ("sql", "SELECT name FROM users".to_string()),
            ],
        );
    }

    #[test]
    fn rejects_prose_and_non_sql_strings() {
        // Statement-shaped keyword sequences only: bare `update` /
        // `delete` / `create` prefixes are prose, not SQL.
        let src = br#"package main

func f() []string {
	return []string{
		"update the docs",
		"delete this file",
		"create a branch",
		"insert coin",
		"with great power",
		"plain text",
	}
}
"#;
        assert_eq!(detect_all(src), vec![]);
    }

    #[test]
    fn sites_are_anchored_at_the_literal() {
        // Probing an ANCESTOR of the string literal must not yield the
        // site — otherwise the fold would emit it once per ancestor.
        let src = b"package main\n\nvar q = `SELECT name FROM users`\n";
        let tree = parse_go(src);
        let engine = injection_engine(TsLanguage::Go).unwrap();
        assert!(
            engine.sites(&tree.root_node(), src).is_empty(),
            "source_file root must not anchor a string-literal pattern"
        );
        assert_eq!(detect_all(src).len(), 1, "the literal itself must");
    }

    #[test]
    fn empty_string_content_never_injects() {
        // `` and "" have no content node; a content-less literal must
        // not produce a zero-length site.
        let src = b"package main\n\nvar a = \"\"\n";
        assert_eq!(detect_all(src), vec![]);
    }

    #[test]
    fn injection_epoch_is_stable_and_content_sensitive() {
        // Deterministic within a binary...
        let a = current_injection_epoch();
        let b = current_injection_epoch();
        assert_eq!(a, b, "composite must be deterministic");
        assert_eq!(a.len(), 64, "full hex of a 32-byte σ hash");
        // ...and sensitive to the injections.scm bytes. Env mutation is
        // process-global; this test is the only one in this binary
        // touching LLO_INJECTIONS_SCM, and it restores on exit.
        // SAFETY: no concurrent env access for this key in this crate's
        // test binary.
        unsafe { std::env::set_var("LLO_INJECTIONS_SCM", "; other content") };
        let c = current_injection_epoch();
        unsafe { std::env::remove_var("LLO_INJECTIONS_SCM") };
        assert_ne!(
            a, c,
            "different injections.scm bytes must change the composite"
        );
        assert_eq!(
            a,
            current_injection_epoch(),
            "removing the override must restore the compiled-in composite"
        );
    }
}
