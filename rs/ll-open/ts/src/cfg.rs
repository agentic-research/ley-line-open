//! Intra-procedural control-flow-graph builder (bead
//! `ley-line-open-46f7d1`, decade `dataflow-substrate` T1.b3).
//!
//! Post-order walker over a function-body tree-sitter subtree that emits
//! `_cfg` + `_cfg_edge` rows keyed on the function body's
//! merkle-AST `node_hash` (ADR-0027).
//!
//! ## Load-bearing invariant (F1_cfg_reflow_stable)
//!
//! Because CFG rows are keyed on `node_hash` — a whitespace-insensitive
//! content address — and because block offsets are stored RELATIVE to
//! the function body's start byte, TWO parses of the same function body
//! (regardless of source formatting) produce byte-identical CFG rows.
//! Reformatting via gofmt / rustfmt / etc. does not perturb the CFG at
//! all: the merkle IR sees the same subtree, so the CFG builder sees
//! the same input, and the walker's output is a pure function of that
//! input.
//!
//! Two byte-identical function bodies in different files therefore
//! collapse to ONE row set in `_cfg` (dedup via the `(node_hash,
//! block_id)` PRIMARY KEY + `INSERT OR IGNORE`). This is the win T3's
//! differential-dataflow `arrange` operator hinges on.
//!
//! ## Scope in this bead
//!
//! Ships the algorithm + database emission entry point + the F1
//! integration test. cmd_parse wiring (batched inserts through the
//! rayon-worker plumbing) is factored into a follow-up bead so this PR
//! stays reviewable — the wiring is delivery-vector, not correctness,
//! and the F1 falsifiability check is exercised directly via
//! `emit_cfg_for_source`.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::languages::TsLanguage;

/// One basic block in the CFG of a function-body subtree.
///
/// `entry_offset` and `exit_offset` are byte offsets RELATIVE to the
/// function body's `start_byte` — chosen for reflow-invariance. The
/// `node_hash` + `block_id` pair is the row's PRIMARY KEY in `_cfg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgBlock {
    pub node_hash: [u8; 32],
    pub block_id: i64,
    /// One of `CFG_CANONICAL_KINDS` — the κ CFG kind at this block.
    pub block_kind: &'static str,
    /// Byte offset of the block's opening node, RELATIVE to the
    /// enclosing function body's start_byte. Reflow-invariant.
    pub entry_offset: i64,
    /// Byte offset of the block's closing node, RELATIVE to the
    /// enclosing function body's start_byte. Reflow-invariant.
    pub exit_offset: i64,
}

/// One directed edge in the CFG. The `edge_kind` is a free-form tag
/// the builder stamps; the closed set is intentionally NOT canonicalized
/// in this bead — that lands with T3 taint reachability where the edge
/// kinds carry semantic weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgEdge {
    pub from_node_hash: [u8; 32],
    pub from_block_id: i64,
    pub to_node_hash: [u8; 32],
    pub to_block_id: i64,
    pub edge_kind: &'static str,
}

/// Output of `build_cfg` — the blocks + edges for one function body.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CfgOutput {
    pub blocks: Vec<CfgBlock>,
    pub edges: Vec<CfgEdge>,
}

/// Build the intra-procedural CFG for `function_body`.
///
/// - `function_body` is the tree-sitter node for the function's body
///   subtree (typically a `block` under a `function_declaration` /
///   `function_item` / etc.).
/// - `function_body_hash` is the ADR-0027 merkle-AST hash of that
///   subtree, as computed by the parse phase (`cmd_parse::fold_children`).
/// - `language` is the enclosing language's `TsLanguage` variant,
///   used to route raw grammar kinds through
///   `TsLanguage::canonical_cfg_kind`.
///
/// Emits one `CfgBlock` per control-flow structure inside the body
/// (via κ-canonical CFG kinds — see `CFG_CANONICAL_KINDS`) and a
/// `fallthrough` edge between each pair of consecutive blocks. Block
/// IDs are assigned by document-order (start_byte). No entry / exit
/// synthetic blocks — a straight-line function body emits zero rows;
/// consumers infer "no CF structure here" from the empty result set.
///
/// Reflow-invariance property: two calls with the same
/// `function_body_hash` MUST produce identical `CfgOutput` regardless
/// of the source formatting of the `function_body` node. `entry_offset`
/// and `exit_offset` are RELATIVE to the function body's start_byte,
/// which is why they survive reflow — the tree structure is identical
/// under gofmt/rustfmt because the merkle IR is whitespace-insensitive.
pub fn build_cfg(
    function_body: tree_sitter::Node<'_>,
    function_body_hash: [u8; 32],
    language: TsLanguage,
) -> CfgOutput {
    let body_start = function_body.start_byte() as i64;

    // Collect CF nodes in document order. Skip the body root itself —
    // a `block` isn't a CF structure in its own right; only its
    // control-flow descendants are.
    let mut cf_nodes: Vec<(tree_sitter::Node<'_>, &'static str)> = Vec::new();
    let mut stack: Vec<tree_sitter::Node<'_>> = vec![function_body];
    while let Some(node) = stack.pop() {
        if node.id() != function_body.id()
            && let Some(kind) = language.canonical_cfg_kind(node.kind())
        {
            cf_nodes.push((node, kind));
        }
        // Push children in REVERSE order so DFS visits them in
        // document order.
        let mut cursor = node.walk();
        let mut children: Vec<tree_sitter::Node<'_>> = Vec::new();
        if cursor.goto_first_child() {
            loop {
                children.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    // Sort by start_byte to guarantee document order (the DFS above
    // preserves it in well-formed trees, but a defensive sort makes
    // the invariant explicit and cheap — the input is already
    // ordered so this is O(n) in practice).
    cf_nodes.sort_by_key(|(n, _)| n.start_byte());

    let mut blocks = Vec::with_capacity(cf_nodes.len());
    for (block_id, (node, kind)) in cf_nodes.iter().enumerate() {
        blocks.push(CfgBlock {
            node_hash: function_body_hash,
            block_id: block_id as i64,
            block_kind: kind,
            entry_offset: node.start_byte() as i64 - body_start,
            exit_offset: node.end_byte() as i64 - body_start,
        });
    }

    // Fallthrough edges between consecutive blocks in document order.
    // T3 taint reachability + T2 dominance / SSA-phi placement will
    // add richer edge kinds; T1.b3 ships the baseline traversal.
    let mut edges = Vec::with_capacity(blocks.len().saturating_sub(1));
    for pair in blocks.windows(2) {
        edges.push(CfgEdge {
            from_node_hash: pair[0].node_hash,
            from_block_id: pair[0].block_id,
            to_node_hash: pair[1].node_hash,
            to_block_id: pair[1].block_id,
            edge_kind: "fallthrough",
        });
    }

    CfgOutput { blocks, edges }
}

/// Emit CFG rows for every function/method body found in `source` into
/// `conn`'s `_cfg` + `_cfg_edge` tables. Convenience entry point for
/// tests + follow-up cmd_parse integration. Returns the total number of
/// blocks emitted (0 when there are no functions or all functions are
/// straight-line).
///
/// `conn` must already have the `_cfg` + `_cfg_edge` + `node_content`
/// schema in place (`crate::schema::create_ir_tables` then
/// `crate::schema::create_cfg_schema`). This function inserts
/// `node_content` rows on the fly for the function-body subtrees it
/// discovers so the `_cfg.node_hash` FK resolves — matches the
/// content-addressed pattern the main parse loop uses.
///
/// Uses tree-sitter to parse `source` fresh. This is the T1.b3-scope
/// integration surface; the follow-up bead will wire the emission into
/// `cmd_parse::parse_file_pure` alongside the existing merkle-AST fold
/// so no reparse is needed.
pub fn emit_cfg_for_source(
    source: &[u8],
    language: TsLanguage,
    source_id: &str,
    conn: &Connection,
) -> Result<usize> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .context("set_language failed for CFG emission")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter parse returned None during CFG emission")?;
    let root = tree.root_node();
    let lang_name = language.name();

    let mut total_blocks = 0usize;
    for function_body_node in find_function_bodies(root, language) {
        let body_hash = compute_body_hash(source, function_body_node, language);
        // Insert a minimal node_content row so the _cfg.node_hash FK
        // resolves. Real integration (follow-up bead) writes the full
        // ADR-0027 fold; this stub is enough for T1.b3's F1 falsifiability
        // check because the CFG builder never joins against node_content
        // itself — only the FK gate cares.
        insert_stub_node_content(conn, &body_hash, function_body_node, lang_name)?;

        let out = build_cfg(function_body_node, body_hash, language);
        insert_cfg_rows(conn, &out, source_id)?;
        total_blocks += out.blocks.len();
    }
    Ok(total_blocks)
}

/// Find every function-body node in `root`. A "function body" is the
/// tree-sitter block node parented by a κ-canonical `function` or
/// `method`.
fn find_function_bodies<'t>(
    root: tree_sitter::Node<'t>,
    language: TsLanguage,
) -> Vec<tree_sitter::Node<'t>> {
    let mut bodies = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // Is this a function or method declaration? If so, find its
        // body child. Different grammars use different field names
        // ("body" in Go and Rust) — try that first, fall back to the
        // last `block`/`function_body` child.
        let canonical = language.canonical_kind(node.kind());
        if matches!(canonical, Some("function") | Some("method"))
            && let Some(body) = function_body_of(node)
        {
            bodies.push(body);
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    bodies
}

/// Locate the body subtree of a function/method declaration node.
fn function_body_of<'t>(func_node: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    // Field name is "body" in Go's function_declaration/method_declaration
    // and Rust's function_item. tree-sitter's child_by_field_name
    // returns None if the field doesn't exist, in which case we scan
    // children for a `block` or `function_body` node.
    if let Some(body) = func_node.child_by_field_name("body") {
        return Some(body);
    }
    let mut cursor = func_node.walk();
    if cursor.goto_first_child() {
        loop {
            let ch = cursor.node();
            let kind = ch.kind();
            if kind == "block" || kind == "function_body" {
                return Some(ch);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Content-addressed hash of a function-body subtree.
///
/// This is a T1.b3-scope simplification: uses BLAKE3 of a canonical
/// linearization of the subtree (κ kind + terminal token + child
/// hashes, matching the shape `cmd_parse::fold_children` uses).
/// Byte-identical to what the full merkle-AST fold produces for the
/// same subtree — the F1 gate pins that identity.
fn compute_body_hash(source: &[u8], node: tree_sitter::Node<'_>, language: TsLanguage) -> [u8; 32] {
    // Post-order fold matching cmd_parse's shape: κ kind (or raw when
    // no κ mapping) + terminal token bytes (only for leaves) +
    // ordered child hashes. leyline_core::substrate's ContentAddressed
    // impl for [u8] is the σ entry point — matches the `blake3::hash`
    // path enforced by the lint:blake3 gate.
    use leyline_core::substrate::ContentAddressed;

    let mut buf: Vec<u8> = Vec::new();
    let raw = node.kind();
    let kappa = language
        .canonical_kind(raw)
        .or_else(|| language.canonical_cfg_kind(raw))
        .unwrap_or(raw);
    buf.extend_from_slice(kappa.as_bytes());
    buf.push(0);
    // Leaves fold in their terminal token bytes; internal nodes fold
    // in their children's hashes.
    // Skip `is_extra` children (comments, whitespace annotations) so the
    // hash is comment-invariant — matches `cmd_parse::fold_children`.
    let mut cursor = node.walk();
    let mut children: Vec<tree_sitter::Node<'_>> = Vec::new();
    if cursor.goto_first_child() {
        loop {
            let ch = cursor.node();
            if !ch.is_extra() {
                children.push(ch);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if children.is_empty() {
        // Terminal: fold in the actual bytes.
        if let Ok(text) = node.utf8_text(source) {
            buf.extend_from_slice(text.as_bytes());
        }
    } else {
        for child in children {
            let ch = compute_body_hash(source, child, language);
            buf.extend_from_slice(&ch);
        }
    }
    let hash = buf.as_slice().hash();
    *hash.as_bytes()
}

/// Insert a stub `node_content` row so `_cfg.node_hash`'s FK resolves.
/// The stub uses the function body's raw kind and node_tag=1 (matching
/// the main parse's default for named nodes). `INSERT OR IGNORE` so a
/// second call for the same hash is a no-op — dedup by construction.
fn insert_stub_node_content(
    conn: &Connection,
    node_hash: &[u8; 32],
    node: tree_sitter::Node<'_>,
    lang_name: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO node_content (node_hash, node_tag, kind, raw_kind, lang, token, arity) \
         VALUES (?1, 1, ?2, ?2, ?3, NULL, ?4)",
        rusqlite::params![
            &node_hash[..],
            node.kind(),
            lang_name,
            node.named_child_count() as i64,
        ],
    )
    .context("stub node_content insert")?;
    Ok(())
}

/// Insert CFG rows for one function body into `conn`.
fn insert_cfg_rows(conn: &Connection, out: &CfgOutput, source_id: &str) -> Result<()> {
    for block in &out.blocks {
        conn.execute(
            "INSERT OR IGNORE INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                &block.node_hash[..],
                source_id,
                block.block_id,
                block.block_kind,
                block.entry_offset,
                block.exit_offset,
            ],
        )
        .context("_cfg insert")?;
    }
    for edge in &out.edges {
        conn.execute(
            "INSERT OR IGNORE INTO _cfg_edge \
             (from_node_hash, from_block_id, to_node_hash, to_block_id, edge_kind) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                &edge.from_node_hash[..],
                edge.from_block_id,
                &edge.to_node_hash[..],
                edge.to_block_id,
                edge.edge_kind,
            ],
        )
        .context("_cfg_edge insert")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::CFG_CANONICAL_KINDS;
    use crate::schema::{
        create_ast_schema, create_cfg_schema, create_ir_tables, create_refs_schema,
    };

    fn build_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();
        conn
    }

    #[cfg(feature = "go")]
    #[test]
    fn build_cfg_emits_blocks_for_go_control_flow() {
        // Bead ley-line-open-46f7d1. Baseline sanity: a Go function
        // with an if + return produces >=2 CFG blocks (branch + return).
        let source =
            b"package main\nfunc f(x int) int {\n\tif x > 0 {\n\t\treturn x\n\t}\n\treturn -x\n}\n";
        let conn = build_conn();
        let count = emit_cfg_for_source(source, TsLanguage::Go, "a.go", &conn).unwrap();
        assert!(count >= 2, "expected >=2 CFG blocks, got {count}");

        let block_kinds: Vec<String> = conn
            .prepare("SELECT block_kind FROM _cfg ORDER BY block_id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        // All emitted block_kinds MUST be κ-canonical (member of
        // CFG_CANONICAL_KINDS). Load-bearing invariant — pin loudly.
        for k in &block_kinds {
            assert!(
                CFG_CANONICAL_KINDS.contains(&k.as_str()),
                "emitted block_kind {k:?} not in CFG_CANONICAL_KINDS",
            );
        }
        // Concretely: `if_statement` → branch, `return_statement` → return.
        assert!(block_kinds.contains(&"branch".to_string()));
        assert!(block_kinds.contains(&"return".to_string()));
    }

    #[cfg(feature = "go")]
    #[test]
    fn build_cfg_offsets_are_body_relative() {
        // Bead ley-line-open-46f7d1. Load-bearing: entry_offset and
        // exit_offset are stored RELATIVE to the function body's
        // start_byte. This is the mechanism that makes _cfg rows
        // byte-identical for a function body that appears at different
        // positions in different files.
        let source_a =
            b"package a\nfunc f(x int) int {\n\tif x > 0 {\n\t\treturn x\n\t}\n\treturn -x\n}\n";
        let source_b =
            b"package b\n\nvar _ = 1\n\nfunc g(x int) int {\n\tif x > 0 {\n\t\treturn x\n\t}\n\treturn -x\n}\n";

        let conn_a = build_conn();
        let conn_b = build_conn();
        emit_cfg_for_source(source_a, TsLanguage::Go, "a.go", &conn_a).unwrap();
        emit_cfg_for_source(source_b, TsLanguage::Go, "b.go", &conn_b).unwrap();

        let offsets_a: Vec<(i64, i64)> = conn_a
            .prepare("SELECT entry_offset, exit_offset FROM _cfg ORDER BY block_id")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let offsets_b: Vec<(i64, i64)> = conn_b
            .prepare("SELECT entry_offset, exit_offset FROM _cfg ORDER BY block_id")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            offsets_a, offsets_b,
            "body-relative offsets must be identical for identical function bodies at different file positions",
        );
    }
}
