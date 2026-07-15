//! Cross-reference extraction from tree-sitter AST nodes.
//!
//! Language-specific extractors produce `ExtractedRef` values.
//! The caller decides how to store them (SQLite, vector, etc.).

use tree_sitter::Node;

/// Version of the extraction rules — the `extract_*` emission behavior
/// that derives `node_defs` / `node_refs` / `_imports` from an AST.
///
/// `node_hash` is a fold over source bytes only, so a rules change with
/// unchanged sources is invisible to the merkle/sheaf invalidation
/// layer (v0.7.8 hit this: keyed_element/argument_list value-position
/// refs changed `extract_go` output for byte-identical files, and
/// existing arenas kept serving the old rows). The parse layer stores
/// this epoch in `_meta.extraction_epoch` and forces full fact
/// re-derivation when the stored value disagrees with the binary's —
/// see bead `ley-line-open-20988a`.
///
/// Bump whenever ANY `extract_*` emission behavior changes. When
/// queries-as-data lands (bead `ley-line-open-206d53`) this constant is
/// superseded by the query_set_hash computed over the loaded query set.
///
/// Deliberately NOT folded into `node_hash` itself — that would couple
/// parse identity to extraction version and kill content dedup across
/// versions.
///
/// History:
/// - 1: initial epoch (bead `ley-line-open-20988a`).
/// - 2: Tier 3 queries for java/c/cpp (bead `ley-line-open-5e21c2`).
///   These languages previously emitted NOTHING, so no existing rows
///   are wrong — but the epoch gate is what forces the re-derivation
///   pass that picks the new java/c/cpp facts up on binary upgrade;
///   without the bump, an existing arena keeps serving zero symbols
///   for those files forever.
/// - 3: Tier 3 partial algebra for sql/bash (bead
///   `ley-line-open-780821`). Same silent-empty shape as epoch 2:
///   both languages previously emitted nothing, and the bump forces
///   existing arenas to re-derive so .sql/.sh files gain their
///   def/ref/import rows on binary upgrade.
/// - 4: structural `qualifier` on node_refs (bead
///   `ley-line-open-4dde42`). The bare-token row of a qualified call's
///   dual-emit pair now carries the receiver/selector text in a new
///   nullable `node_refs.qualifier` column. Emission change with
///   byte-identical sources — without the bump, an upgraded arena keeps
///   serving all-NULL qualifiers forever.
pub const EXTRACTION_EPOCH: u64 = 4;

/// Effective extraction epoch: `LLO_EXTRACTION_EPOCH` overrides the
/// compile-time constant so one test binary can act as two releases
/// with different extraction rules. Unset or non-numeric values fall
/// back to [`EXTRACTION_EPOCH`].
pub fn current_extraction_epoch() -> u64 {
    std::env::var("LLO_EXTRACTION_EPOCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(EXTRACTION_EPOCH)
}

/// A single extracted reference, definition, or import.
///
/// Universal across languages — Go, Python, JS, etc. all produce these.
/// The extraction function is language-specific; the data type is not.
///
/// `container_node_id` (bead `ley-line-open-6e798d`) is the node_id of
/// the nearest enclosing κ function/method ancestor. `None` for
/// top-level refs/defs (file-scope declarations, imports). Consumers
/// use this to `GROUP BY container_node_id` for per-caller aggregation
/// without a recursive `nodes.parent_id` walk — the primary consumer
/// today is mache's smell rules (fan_out_skew, untested_function).
#[derive(Debug, Clone)]
pub enum ExtractedRef {
    /// A function/method/type definition.
    ///
    /// `canonical_kind` is the κ canonical kind of the definition
    /// (`function` / `method` / `type` / `constant` / `variable` /
    /// `field` / `module` / `import` / `parameter`), per
    /// `TsLanguage::canonical_kind`. `None` when the raw grammar kind
    /// has no κ mapping (open-world escape). Cross-repo follow-up to
    /// bead `ley-line-open-6e798d` — mache's `dead_code` and
    /// `god_file` rules filter by symbol-scope κ kind, and having the
    /// column on `node_defs` avoids a JOIN through
    /// `node_content.kind` per rule.
    Def {
        token: String,
        node_id: String,
        source_id: String,
        container_node_id: Option<String>,
        canonical_kind: Option<&'static str>,
    },
    /// A call-site reference.
    ///
    /// `qualifier` (bead `ley-line-open-4dde42`) is the syntactic
    /// receiver/selector text of a qualified call site, carried on the
    /// BARE-token row of the engine's dual-emit pair (`fmt.Println(..)`
    /// → the `Println` row carries `Some("fmt")`). The qualified-token
    /// row (`fmt.Println`) and genuinely bare calls carry `None` —
    /// exactly one row per qualified call site holds the structural
    /// (name, qualifier) pair, so consumers (mache's `fatal_call`
    /// qualifier JOIN `_imports.alias`, `fan_out_skew`'s mention arm)
    /// can GROUP BY/filter without string-splitting tokens.
    Ref {
        token: String,
        node_id: String,
        source_id: String,
        container_node_id: Option<String>,
        qualifier: Option<String>,
    },
    /// An import alias→path mapping. No `container_node_id` — imports
    /// are file-scope by construction; a "container" is not
    /// well-defined for them.
    Import {
        alias: String,
        path: String,
        source_id: String,
    },
}

/// Insert extracted refs into SQLite tables.
///
/// Universal — works with output from any language extractor.
pub fn insert_extracted_refs(
    conn: &rusqlite::Connection,
    refs: &[ExtractedRef],
) -> anyhow::Result<()> {
    for r in refs {
        match r {
            ExtractedRef::Def {
                token,
                node_id,
                source_id,
                container_node_id,
                canonical_kind,
            } => crate::schema::insert_def(
                conn,
                token,
                node_id,
                source_id,
                container_node_id.as_deref(),
                *canonical_kind,
            )?,
            ExtractedRef::Ref {
                token,
                node_id,
                source_id,
                container_node_id,
                qualifier,
            } => crate::schema::insert_ref(
                conn,
                token,
                node_id,
                source_id,
                container_node_id.as_deref(),
                qualifier.as_deref(),
            )?,
            ExtractedRef::Import {
                alias,
                path,
                source_id,
            } => crate::schema::insert_import(conn, alias, path, source_id)?,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Language dispatcher (factory)
// ---------------------------------------------------------------------------

/// Extract refs/defs/imports from an AST node, dispatching by language.
///
/// Unsupported languages return an empty vec (no refs, no error).
/// Add new languages by adding a match arm here + an `extract_<lang>` function.
//
// `#[allow(unused_variables)]`: every match arm is feature-gated, so when
// no language with a refs extractor is enabled the parameters are unused.
// They're load-bearing when any extractor feature is on.
#[allow(unused_variables)]
pub fn extract_refs(
    node: &tree_sitter::Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    language: crate::languages::TsLanguage,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    match language {
        #[cfg(feature = "go")]
        crate::languages::TsLanguage::Go => {
            extract_go(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "rust")]
        crate::languages::TsLanguage::Rust => {
            extract_rust(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "python")]
        crate::languages::TsLanguage::Python => {
            extract_python(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "javascript")]
        crate::languages::TsLanguage::JavaScript => {
            extract_javascript(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "typescript")]
        crate::languages::TsLanguage::TypeScript => {
            extract_typescript(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "java")]
        crate::languages::TsLanguage::Java => {
            extract_java(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "c")]
        crate::languages::TsLanguage::C => {
            extract_c(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "cpp")]
        crate::languages::TsLanguage::Cpp => {
            extract_cpp(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "sql")]
        crate::languages::TsLanguage::Sql => {
            extract_sql(node, source, node_id, source_id, container_node_id)
        }
        #[cfg(feature = "bash")]
        crate::languages::TsLanguage::Bash => {
            extract_bash(node, source, node_id, source_id, container_node_id)
        }
        _ => Vec::new(),
    }
}

/// Effective per-language extraction for the content-addressing fold:
/// an arena-resident TRUSTED override engine when `queries` carries one
/// for `language`, else the compiled-in default (via [`extract_refs`]).
/// Bead `ley-line-open-e72629`.
///
/// An override REPLACES the compiled `.scm`-driven emission; the
/// query-inexpressible imperative arms above (`extract_rust` use-lists,
/// `extract_python` from-imports, qualified `Class.method` defs, JS/TS κ
/// fixups) do NOT re-run under an override — an operator shipping an
/// override owns the complete emission for that language. Lossless for
/// the pure-delegate languages (Go / C / SQL / Bash); see the module
/// header of [`crate::query_engine`].
///
/// When an override engine trips its resource bounds on `node`, `bounds`
/// is set and an empty vec is returned — the caller drops this file's
/// facts (no facts for this file), never hangs, never emits partial rows.
#[allow(clippy::too_many_arguments)]
pub fn extract_refs_resolved(
    node: &tree_sitter::Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    language: crate::languages::TsLanguage,
    container_node_id: Option<&str>,
    queries: &crate::query_engine::QuerySet,
    bounds: &std::cell::Cell<bool>,
) -> Vec<ExtractedRef> {
    match queries.override_engine(language) {
        Some(engine) => {
            match engine.extract_bounded(node, source, node_id, source_id, container_node_id) {
                Ok(v) => v,
                Err(_) => {
                    bounds.set(true);
                    Vec::new()
                }
            }
        }
        None => extract_refs(
            node,
            source,
            node_id,
            source_id,
            language,
            container_node_id,
        ),
    }
}

// ---------------------------------------------------------------------------
// Go extractor
// ---------------------------------------------------------------------------

/// Extract Go definitions, call references, and imports from a single AST node.
///
/// Pure data — no database access, safe for parallel use.
///
/// The per-language knowledge lives in `queries/go/tags.scm`; this
/// function compiles it once and delegates to the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine) interpreter.
/// Emission behavior (dual-emit `Receiver.Method`+`Method`,
/// value-position identifier refs, import alias defaulting) is pinned
/// by the fixture tests below — edit the `.scm`, keep the tests green.
#[cfg(feature = "go")]
pub fn extract_go(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::Go,
                include_str!("../queries/go/tags.scm"),
            )
            .expect("compiled-in queries/go/tags.scm must compile against tree-sitter-go")
        })
        .extract(node, source, node_id, source_id, container_node_id)
}

// ---------------------------------------------------------------------------
// Rust extractor
// ---------------------------------------------------------------------------

/// Extract Rust definitions, call references, macro invocations, and `use`
/// imports from a single AST node. Pure data — no DB access.
///
/// The per-language knowledge lives in `queries/rust/tags.scm`,
/// compiled once and interpreted by the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine). Two arms are
/// outside the query→fact vocabulary and stay imperative (bead
/// `ley-line-open-42f2b3`):
///
/// - Qualified `Receiver::method` defs (bead `ley-line-open-caf423`):
///   the receiver is the `type`/`name` field of an ANCESTOR
///   `impl_item`/`trait_item`, and tree-sitter patterns match downward
///   only — no pattern anchored at the function node can capture it.
/// - Use-tree flattening: `use a::{b, c as d, e::{f}}` joins the
///   shared path prefix onto each leaf and recurses to unbounded
///   depth; `@path` reads a single node's text.
///
/// Everything else — defs, call refs (bare / method / `::`-qualified
/// dual-emit), macro refs, single-leaf imports — is pinned by the
/// fixture tests below: edit the `.scm`, keep the tests green.
/// Closures (`closure_expression`) are intentionally NOT matched —
/// they're anonymous, no stable token.
#[cfg(feature = "rust")]
pub fn extract_rust(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let mut out = Vec::new();

    // Upward-context arm: the qualified def precedes the bare def the
    // `.scm` emits, preserving the pre-port emission order.
    if matches!(node.kind(), "function_item" | "function_signature_item")
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(source)
        && !name.is_empty()
        && let Some(recv) = rust_impl_receiver(node, source)
    {
        out.push(ExtractedRef::Def {
            token: format!("{recv}::{name}"),
            node_id: node_id.to_string(),
            source_id: source_id.to_string(),
            container_node_id: container_node_id.map(str::to_string),
            canonical_kind: crate::languages::TsLanguage::Rust.canonical_kind(node.kind()),
        });
    }

    // Use-list arm: single-leaf `use` forms come from the `.scm`; list
    // forms need prefix joining + recursion.
    if node.kind() == "use_declaration"
        && let Some(arg) = node.child_by_field_name("argument")
        && matches!(arg.kind(), "scoped_use_list" | "use_list")
    {
        collect_use_imports(arg, source, source_id, &mut out);
    }

    out.extend(
        ENGINE
            .get_or_init(|| {
                crate::query_engine::QueryEngine::new(
                    crate::languages::TsLanguage::Rust,
                    include_str!("../queries/rust/tags.scm"),
                )
                .expect("compiled-in queries/rust/tags.scm must compile against tree-sitter-rust")
            })
            .extract(node, source, node_id, source_id, container_node_id),
    );
    out
}

/// Recursively flatten a `use` LIST tree into `ExtractedRef::Import`
/// entries. Single-leaf forms (`use foo;`, `use a::b;`,
/// `use a::b as c;`) are query patterns in `queries/rust/tags.scm`;
/// this handles only the list shapes, whose shared-prefix joining and
/// unbounded nesting the query vocabulary cannot express (bead
/// `ley-line-open-42f2b3`).
///
/// Wildcards (`use foo::{a, io::*};`) are skipped — no stable alias to
/// attach.
#[cfg(feature = "rust")]
fn collect_use_imports(
    node: Node<'_>,
    source: &[u8],
    source_id: &str,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        // `use foo::{a, b};` — list children are individual use trees
        // sharing the `foo::` prefix. tree-sitter-rust emits these as a
        // `scoped_use_list` node with `path: foo` and `list: use_list`.
        "scoped_use_list" => {
            let path_prefix = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.named_children(&mut cursor) {
                    collect_use_list_child(child, source, source_id, path_prefix, out);
                }
            }
        }
        // `use {a, b};` (rare: top-level un-prefixed list).
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_use_list_child(child, source, source_id, "", out);
            }
        }
        _ => {}
    }
}

/// Helper for items inside a `use_list` — the leaf may be a bare ident
/// (`a` → `foo::a`), a scoped ident (`a::b` → `foo::a::b`), or an alias
/// clause (`a as renamed` → `foo::a` with alias=`renamed`).
#[cfg(feature = "rust")]
fn collect_use_list_child(
    node: Node<'_>,
    source: &[u8],
    source_id: &str,
    path_prefix: &str,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(source).unwrap_or("");
            if name.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                name.to_string()
            } else {
                format!("{path_prefix}::{name}")
            };
            out.push(ExtractedRef::Import {
                alias: name.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        "scoped_identifier" => {
            let leaf_path = node.utf8_text(source).unwrap_or("");
            let alias = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if leaf_path.is_empty() || alias.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                leaf_path.to_string()
            } else {
                format!("{path_prefix}::{leaf_path}")
            };
            out.push(ExtractedRef::Import {
                alias: alias.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        "use_as_clause" => {
            let leaf_path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let alias = node
                .child_by_field_name("alias")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if leaf_path.is_empty() || alias.is_empty() {
                return;
            }
            let full = if path_prefix.is_empty() {
                leaf_path.to_string()
            } else {
                format!("{path_prefix}::{leaf_path}")
            };
            out.push(ExtractedRef::Import {
                alias: alias.to_string(),
                path: full,
                source_id: source_id.to_string(),
            });
        }
        // Nested lists: `use foo::{bar::{a, b}};`
        "scoped_use_list" | "use_list" => {
            collect_use_imports(node, source, source_id, out);
        }
        // self / wildcard inside a list — skip.
        _ => {}
    }
}

/// When `func_node` (a `function_item` / `function_signature_item`) is
/// nested inside an `impl_item` or `trait_item`, return the receiver's
/// text. Returns `None` when the parent chain doesn't match the
/// `function_item → declaration_list → impl_item|trait_item` shape (bare
/// top-level function, function nested inside another function, etc.).
///
/// Bead `ley-line-open-caf423`. tree-sitter-rust's `impl_item` node has
/// a `type` field carrying the impl'd type (e.g. `S` in `impl S {…}` or
/// `Vec<u8>` in `impl Vec<u8> {…}`). `trait_item` carries a `name` field
/// with the trait's identifier (e.g. `Greet` in `trait Greet {…}`). For
/// impl blocks we take the raw type text — a generics-bearing type
/// qualifies as `Vec<u8>::foo`, which is the least-surprising round-trip.
/// For trait blocks the receiver is the trait name itself so a default
/// method `hello` inside `trait Greet` qualifies as `Greet::hello`,
/// mirroring the docstring claim on `extract_rust`.
#[cfg(feature = "rust")]
fn rust_impl_receiver(func_node: &Node, source: &[u8]) -> Option<String> {
    let list = func_node.parent()?;
    if list.kind() != "declaration_list" {
        return None;
    }
    let container = list.parent()?;
    let field = match container.kind() {
        "impl_item" => "type",
        "trait_item" => "name",
        _ => return None,
    };
    let recv_node = container.child_by_field_name(field)?;
    let recv = recv_node.utf8_text(source).ok()?;
    if recv.is_empty() {
        None
    } else {
        Some(recv.to_string())
    }
}

// ---------------------------------------------------------------------------
// Python extractor
// ---------------------------------------------------------------------------

/// Extract Python definitions, call references, and imports from a
/// single AST node.
///
/// Pure data — no database access, safe for parallel use.
///
/// The per-language knowledge lives in `queries/python/tags.scm`; this
/// function compiles it once and delegates to the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine) interpreter (bead
/// `ley-line-open-426dfd`, following the Go port in bead
/// `ley-line-open-206d53`). Two arms the engine's vocabulary cannot
/// express stay imperative in the match below — qualified
/// `Class.method` defs (ancestor qualifier) and `import_from_statement`
/// (joined path); each carries its own leak note. Emission behavior
/// (bead `ley-line-open-caf423`) is pinned by cli-lib's
/// `def_ref_extraction_fidelity_test` — edit the `.scm`, keep the
/// fixtures green.
#[cfg(feature = "python")]
pub fn extract_python(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let engine = ENGINE.get_or_init(|| {
        crate::query_engine::QueryEngine::new(
            crate::languages::TsLanguage::Python,
            include_str!("../queries/python/tags.scm"),
        )
        .expect("compiled-in queries/python/tags.scm must compile against tree-sitter-python")
    });

    let mut out = Vec::new();
    match node.kind() {
        // Query-inexpressible leak 1: the qualified `Class.method` def.
        // The qualifier is an ANCESTOR of the anchored node
        // (function_definition → block → class_definition) and
        // tree-sitter queries match downward only. A pattern rooted at
        // the class instead would emit under the CLASS's node_id and
        // canonical_kind, not the method's. The engine emits the bare
        // `method` def; this arm prepends the qualified form —
        // qualified-first ordering, same as the engine's own dual-emit.
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(source)
                && !name.is_empty()
                && let Some(cls) = python_enclosing_class(node, source)
            {
                out.push(ExtractedRef::Def {
                    token: format!("{cls}.{name}"),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind: crate::languages::TsLanguage::Python
                        .canonical_kind(node.kind()),
                });
            }
        }
        // Query-inexpressible leak 2: from-imports. The emitted path is
        // a `{module}.{name}` JOIN of two captures; the engine's import
        // vocabulary carries exactly one @path (quote-trim +
        // `/`-segment alias defaulting) and has no join. `from mod
        // import a, b as c` — the `module_name` field carries the path
        // prefix; every non-module child is an import target.
        "import_from_statement" => {
            let prefix = node
                .child_by_field_name("module_name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "dotted_name" {
                    // The module_name itself is a `dotted_name`; only
                    // the *imported* names emit here.
                    if child.utf8_text(source).unwrap_or("") == prefix {
                        continue;
                    }
                    let name = child.utf8_text(source).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let path = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    out.push(ExtractedRef::Import {
                        alias: name.to_string(),
                        path,
                        source_id: source_id.to_string(),
                    });
                } else if child.kind() == "aliased_import" {
                    let name = child
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    let alias = child
                        .child_by_field_name("alias")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");
                    if name.is_empty() || alias.is_empty() {
                        continue;
                    }
                    let path = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    out.push(ExtractedRef::Import {
                        alias: alias.to_string(),
                        path,
                        source_id: source_id.to_string(),
                    });
                }
            }
        }
        _ => {}
    }
    out.extend(engine.extract(node, source, node_id, source_id, container_node_id));
    out
}

/// Return the enclosing `class_definition`'s name when `func_node` (a
/// `function_definition`) is a method. Returns `None` for module-level
/// functions or functions nested inside other functions. Bead
/// `ley-line-open-caf423`.
#[cfg(feature = "python")]
fn python_enclosing_class(func_node: &Node, source: &[u8]) -> Option<String> {
    // function_definition → block → class_definition
    let block = func_node.parent()?;
    if block.kind() != "block" {
        return None;
    }
    let cls = block.parent()?;
    if cls.kind() != "class_definition" {
        return None;
    }
    let name = cls.child_by_field_name("name")?.utf8_text(source).ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ---------------------------------------------------------------------------
// JavaScript extractor
// ---------------------------------------------------------------------------

/// Extract JavaScript definitions, call references, and imports from a
/// single AST node. Pure data — no DB access, safe for parallel use.
///
/// The per-language knowledge lives in `queries/javascript/tags.scm`;
/// this function compiles it once and delegates to the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine) interpreter, then
/// applies [`js_ts_context_fixups`] for the two facts an anchored
/// downward-matching query cannot express (qualified `Class.method`
/// defs, κ = "function" on var-bound arrows/function expressions).
/// Emission behavior is pinned by the fixture tests in
/// `rs/ll-open/cli-lib/tests/def_ref_extraction_fidelity_test.rs` —
/// edit the `.scm`, keep the fixtures green.
#[cfg(feature = "javascript")]
pub fn extract_javascript(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let mut out = ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::JavaScript,
                include_str!("../queries/javascript/tags.scm"),
            )
            .expect(
                "compiled-in queries/javascript/tags.scm must compile against tree-sitter-javascript",
            )
        })
        .extract(node, source, node_id, source_id, container_node_id);
    js_ts_context_fixups(
        node,
        source,
        crate::languages::TsLanguage::JavaScript,
        &mut out,
    );
    out
}

/// Post-pass for the two facts the anchored query engine cannot
/// express for JS/TS (bead `ley-line-open-451f77`); shared because the
/// TSX grammar reuses the JS node kinds for both constructs.
///
/// 1. Qualified `Class.method` defs: query patterns match downward
///    from their root, so a pattern anchored at `method_definition`
///    cannot capture the ANCESTOR class name. This walks
///    `method_definition` → `class_body` → class parent and prepends
///    the qualified form of the engine's bare-name def (qualified
///    first, bare second — same dual-emit order as the engine's own
///    `@qualifier` rule). `abstract_class_declaration` exists only in
///    the TSX grammar; the kind check is inert for JS.
/// 2. κ for var-bound functions: the engine derives `canonical_kind`
///    from the ANCHOR node's kind, but a `lexical_declaration` /
///    `variable_declaration` anchor records the definition of the
///    FUNCTION it binds, not of a variable — κ pins to "function"
///    (mache's `dead_code` / `god_file` rules filter on symbol-scope
///    κ; bead `ley-line-open-caf423`).
#[cfg(any(feature = "javascript", feature = "typescript"))]
fn js_ts_context_fixups(
    node: &Node,
    source: &[u8],
    ts_lang: crate::languages::TsLanguage,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        "method_definition" => {
            let Some(cls) = js_ts_enclosing_class(node, source) else {
                return;
            };
            // The engine's only method_definition pattern emits exactly
            // one bare-name def; an empty/suppressed name emits nothing
            // and the qualified form is suppressed with it.
            let Some(ExtractedRef::Def {
                token,
                node_id,
                source_id,
                container_node_id,
                ..
            }) = out.first()
            else {
                return;
            };
            let qualified = ExtractedRef::Def {
                token: format!("{cls}.{token}"),
                node_id: node_id.clone(),
                source_id: source_id.clone(),
                container_node_id: container_node_id.clone(),
                canonical_kind: ts_lang.canonical_kind(node.kind()),
            };
            out.insert(0, qualified);
        }
        "lexical_declaration" | "variable_declaration" => {
            for r in out {
                if let ExtractedRef::Def { canonical_kind, .. } = r {
                    *canonical_kind = Some("function");
                }
            }
        }
        _ => {}
    }
}

/// Enclosing class name for a `method_definition`:
/// `method_definition` → `class_body` → `class_declaration` |
/// `abstract_class_declaration`. Object-literal and class-expression
/// methods return `None` — they emit bare-name defs only. Bead
/// `ley-line-open-caf423`.
#[cfg(any(feature = "javascript", feature = "typescript"))]
fn js_ts_enclosing_class(method_node: &Node, source: &[u8]) -> Option<String> {
    let body = method_node.parent()?;
    if body.kind() != "class_body" {
        return None;
    }
    let cls = body.parent()?;
    if cls.kind() != "class_declaration" && cls.kind() != "abstract_class_declaration" {
        return None;
    }
    let name = cls.child_by_field_name("name")?.utf8_text(source).ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ---------------------------------------------------------------------------
// TypeScript extractor
// ---------------------------------------------------------------------------

/// Extract TypeScript definitions, call references, and imports from a
/// single AST node. Pure data — no DB access, safe for parallel use.
///
/// The per-language knowledge lives in `queries/typescript/tags.scm`,
/// compiled against the TSX grammar (a superset of the JS grammar's
/// node kinds — the query file is the JavaScript query plus the
/// TS-only definition patterns: `interface_declaration`,
/// `type_alias_declaration`, `enum_declaration`,
/// `abstract_class_declaration`). This function compiles it once and
/// delegates to the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine) interpreter, then
/// applies [`js_ts_context_fixups`] for the two facts an anchored
/// downward-matching query cannot express (qualified `Class.method`
/// defs, κ = "function" on var-bound arrows/function expressions).
/// Emission behavior is pinned by the fixture tests in
/// `rs/ll-open/cli-lib/tests/def_ref_extraction_fidelity_test.rs` —
/// edit the `.scm`, keep the fixtures green.
#[cfg(feature = "typescript")]
pub fn extract_typescript(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let mut out = ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::TypeScript,
                include_str!("../queries/typescript/tags.scm"),
            )
            .expect(
                "compiled-in queries/typescript/tags.scm must compile against tree-sitter-typescript (TSX)",
            )
        })
        .extract(node, source, node_id, source_id, container_node_id);
    js_ts_context_fixups(
        node,
        source,
        crate::languages::TsLanguage::TypeScript,
        &mut out,
    );
    out
}

// ---------------------------------------------------------------------------
// Java extractor
// ---------------------------------------------------------------------------

/// Extract Java definitions, call references, and imports from a single
/// AST node. Pure data — no DB access, safe for parallel use.
///
/// First query-native language (bead `ley-line-open-5e21c2`): the
/// per-language knowledge lives in `queries/java/tags.scm` with no
/// preceding imperative extractor — the `.scm` is the extractor,
/// compiled once and interpreted by the generic
/// [`QueryEngine`](crate::query_engine::QueryEngine). One arm is
/// outside the anchored-query vocabulary and stays imperative:
/// qualified `Type.method` defs read the type name from an ANCESTOR
/// class/interface/enum/record body, and tree-sitter patterns match
/// downward only — same leak as `rust_impl_receiver` /
/// `python_enclosing_class` / `js_ts_context_fixups`. Emission behavior
/// is pinned by the `java_tests` fixtures below and cli-lib's
/// `def_ref_extraction_fidelity_test` — edit the `.scm`, keep the
/// fixtures green.
#[cfg(feature = "java")]
pub fn extract_java(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let mut out = Vec::new();

    // Upward-context arm: the qualified def precedes the bare def the
    // `.scm` emits — qualified-first ordering, same as the engine's
    // own dual-emit.
    if node.kind() == "method_declaration"
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(source)
        && !name.is_empty()
        && let Some(ty) = java_enclosing_type(node, source)
    {
        out.push(ExtractedRef::Def {
            token: format!("{ty}.{name}"),
            node_id: node_id.to_string(),
            source_id: source_id.to_string(),
            container_node_id: container_node_id.map(str::to_string),
            canonical_kind: crate::languages::TsLanguage::Java.canonical_kind(node.kind()),
        });
    }

    out.extend(
        ENGINE
            .get_or_init(|| {
                crate::query_engine::QueryEngine::new(
                    crate::languages::TsLanguage::Java,
                    include_str!("../queries/java/tags.scm"),
                )
                .expect("compiled-in queries/java/tags.scm must compile against tree-sitter-java")
            })
            .extract(node, source, node_id, source_id, container_node_id),
    );
    out
}

/// Return the enclosing type declaration's name when `method_node` (a
/// `method_declaration`) is a member. Two parent-chain shapes cover
/// every body that can hold a method_declaration:
///
/// - `method_declaration → class_body|interface_body →
///   class|interface|record_declaration` (a record's body is a
///   `class_body`)
/// - `method_declaration → enum_body_declarations → enum_body →
///   enum_declaration`
///
/// Returns `None` when the chain doesn't match (anonymous-class bodies
/// hang off `object_creation_expression`, which has no `name` field —
/// those methods emit bare only). Bead `ley-line-open-5e21c2`.
#[cfg(feature = "java")]
fn java_enclosing_type(method_node: &Node, source: &[u8]) -> Option<String> {
    let body = method_node.parent()?;
    let decl = match body.kind() {
        "class_body" | "interface_body" => body.parent()?,
        "enum_body_declarations" => {
            let enum_body = body.parent()?;
            if enum_body.kind() != "enum_body" {
                return None;
            }
            enum_body.parent()?
        }
        _ => return None,
    };
    if !matches!(
        decl.kind(),
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    ) {
        return None;
    }
    let name = decl.child_by_field_name("name")?.utf8_text(source).ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ---------------------------------------------------------------------------
// C extractor
// ---------------------------------------------------------------------------

/// Extract C definitions, call references, and `#include` imports from
/// a single AST node. Pure data — no DB access, safe for parallel use.
///
/// Query-native language (bead `ley-line-open-5e21c2`): the
/// per-language knowledge lives in `queries/c/tags.scm` — this is a
/// pure engine delegate with no imperative arm. Extraction reads the
/// tree-sitter parse at face value: macro-produced defs are invisible,
/// inactive-`#ifdef` defs still emit (limitation documented in the
/// `.scm` header). Emission behavior is pinned by the `c_tests`
/// fixtures below and cli-lib's `def_ref_extraction_fidelity_test` —
/// edit the `.scm`, keep the fixtures green.
#[cfg(feature = "c")]
pub fn extract_c(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::C,
                include_str!("../queries/c/tags.scm"),
            )
            .expect("compiled-in queries/c/tags.scm must compile against tree-sitter-c")
        })
        .extract(node, source, node_id, source_id, container_node_id)
}

// ---------------------------------------------------------------------------
// C++ extractor
// ---------------------------------------------------------------------------

/// Extract C++ definitions, call references, and `#include` imports
/// from a single AST node. Pure data — no DB access, safe for parallel
/// use.
///
/// Query-native language (bead `ley-line-open-5e21c2`): the
/// per-language knowledge lives in `queries/cpp/tags.scm` (the C query
/// plus class/namespace/`::` patterns), compiled once and interpreted
/// by the generic [`QueryEngine`](crate::query_engine::QueryEngine).
/// One arm is outside the anchored-query vocabulary and stays
/// imperative: qualified `Class::method` defs for IN-CLASS members
/// (declaration or inline definition) read the class name from an
/// ANCESTOR class/struct body, and tree-sitter patterns match downward
/// only — same leak as `rust_impl_receiver` / `python_enclosing_class`
/// / `js_ts_context_fixups`. Out-of-line `Class::method` definitions
/// need no fixup: the qualified_identifier is a CHILD of the
/// declarator, so the `.scm` dual-emits them directly. Preprocessor
/// limitation as in C (documented in the `.scm` header). Emission
/// behavior is pinned by the `cpp_tests` fixtures below and cli-lib's
/// `def_ref_extraction_fidelity_test` — edit the `.scm`, keep the
/// fixtures green.
#[cfg(feature = "cpp")]
pub fn extract_cpp(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    let mut out = ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::Cpp,
                include_str!("../queries/cpp/tags.scm"),
            )
            .expect("compiled-in queries/cpp/tags.scm must compile against tree-sitter-cpp")
        })
        .extract(node, source, node_id, source_id, container_node_id);

    // Upward-context arm: in-class members name themselves with a
    // field_identifier; the `.scm`'s field_identifier pattern emits
    // exactly one bare-name def, and this prepends the qualified form
    // (qualified first, bare second — same dual-emit order as the
    // engine's own `@qualifier` rule). Out-of-line qualified defs
    // never enter: their declarator is a qualified_identifier, not a
    // field_identifier.
    if node.kind() == "function_declarator"
        && node
            .child_by_field_name("declarator")
            .is_some_and(|d| d.kind() == "field_identifier")
        && let Some(cls) = cpp_enclosing_class(node, source)
        && let Some(ExtractedRef::Def {
            token,
            node_id,
            source_id,
            container_node_id,
            canonical_kind,
        }) = out.first()
    {
        let qualified = ExtractedRef::Def {
            token: format!("{cls}::{token}"),
            node_id: node_id.clone(),
            source_id: source_id.clone(),
            container_node_id: container_node_id.clone(),
            canonical_kind: *canonical_kind,
        };
        out.insert(0, qualified);
    }
    out
}

/// Return the enclosing class/struct/union name when `decl_node` (a
/// `function_declarator` naming a field_identifier) is an in-class
/// member. Two parent-chain shapes:
///
/// - inline definition: `function_declarator → function_definition →
///   field_declaration_list → class_specifier|struct_specifier|…`
/// - declaration only: `function_declarator → field_declaration →
///   field_declaration_list → …`
///
/// Returns `None` when the chain doesn't match (anonymous classes have
/// no `name` field; lambdas and function pointers never reach here —
/// their declarators aren't field_identifiers). Bead
/// `ley-line-open-5e21c2`.
#[cfg(feature = "cpp")]
fn cpp_enclosing_class(decl_node: &Node, source: &[u8]) -> Option<String> {
    let member = decl_node.parent()?;
    if !matches!(member.kind(), "function_definition" | "field_declaration") {
        return None;
    }
    let body = member.parent()?;
    if body.kind() != "field_declaration_list" {
        return None;
    }
    let cls = body.parent()?;
    if !matches!(
        cls.kind(),
        "class_specifier" | "struct_specifier" | "union_specifier"
    ) {
        return None;
    }
    let name = cls.child_by_field_name("name")?.utf8_text(source).ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ---------------------------------------------------------------------------
// SQL extractor
// ---------------------------------------------------------------------------

/// Extract SQL DDL definitions and relation/invocation references from
/// a single AST node. Pure data — no DB access, safe for parallel use.
///
/// Query-native language with a PARTIAL algebra BY DESIGN (bead
/// `ley-line-open-780821`): the per-language knowledge lives in
/// `queries/sql/tags.scm` — this is a pure engine delegate. DDL names
/// (table/view/materialized view/function/schema) are defs;
/// FROM/JOIN/UPDATE/INSERT/DELETE targets, function invocations, and
/// trigger edges are refs; there are no imports (SQL has no
/// in-language import construct). Rejected emissions — index/trigger
/// names, columns, DROP/ALTER targets, CTEs — are documented with
/// reasons in the `.scm` header. Emission behavior is pinned by the
/// `sql_tests` fixtures below and cli-lib's
/// `def_ref_extraction_fidelity_test` — edit the `.scm`, keep the
/// fixtures green.
#[cfg(feature = "sql")]
pub fn extract_sql(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::Sql,
                include_str!("../queries/sql/tags.scm"),
            )
            .expect("compiled-in queries/sql/tags.scm must compile against tree-sitter-sequel")
        })
        .extract(node, source, node_id, source_id, container_node_id)
}

// ---------------------------------------------------------------------------
// Bash extractor
// ---------------------------------------------------------------------------

/// Extract shell function definitions, command references, and static
/// `source` imports from a single AST node. Pure data — no DB access,
/// safe for parallel use.
///
/// Query-native language with a PARTIAL algebra BY DESIGN (bead
/// `ley-line-open-780821`): the per-language knowledge lives in
/// `queries/bash/tags.scm` — this is a pure engine delegate. Function
/// definitions are defs; statically-named command invocations are
/// refs; `source`/`.` with a static path is an import. Variables,
/// expansions, dynamic paths, and aliases are rejected with reasons in
/// the `.scm` header. The `.scm` uses `#any-of?`/`#not-any-of?` text
/// predicates — evaluated natively by the Rust binding's
/// `QueryCursor::matches`, so they are per-pattern query data, not
/// engine code. Emission behavior is pinned by the `bash_tests`
/// fixtures below and cli-lib's `def_ref_extraction_fidelity_test` —
/// edit the `.scm`, keep the fixtures green.
#[cfg(feature = "bash")]
pub fn extract_bash(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<crate::query_engine::QueryEngine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            crate::query_engine::QueryEngine::new(
                crate::languages::TsLanguage::Bash,
                include_str!("../queries/bash/tags.scm"),
            )
            .expect("compiled-in queries/bash/tags.scm must compile against tree-sitter-bash")
        })
        .extract(node, source, node_id, source_id, container_node_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Shared fixture probe (bead `ley-line-open-4dde42`): every `(token,
/// qualifier)` pair in `node_refs`. Each language test module pulls
/// this in via `use super::*;` to pin the structural-qualifier shape —
/// bare-token row carries the qualifier text, qualified-token row and
/// genuinely bare calls carry NULL.
#[cfg(test)]
#[allow(dead_code)]
fn refs_with_qualifier(conn: &rusqlite::Connection) -> Vec<(String, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT token, qualifier FROM node_refs ORDER BY token")
        .expect("node_refs must have the qualifier column (bead ley-line-open-4dde42)");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query node_refs (token, qualifier)")
        .map(|r| r.expect("row decode"))
        .collect()
}

#[cfg(test)]
#[cfg(feature = "go")]
mod tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_go(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_go(&child, src, &id, "test.go", None);
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_function_defs() {
        let src = b"package main\n\nfunc Add() {}\nfunc Sub() {}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(defs.contains(&"Add".to_string()));
        assert!(defs.contains(&"Sub".to_string()));
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn extract_call_refs() {
        let src = b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n\tAdd()\n}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(refs.contains(&"Add".to_string()));
        assert!(refs.contains(&"Println".to_string()));
        assert!(refs.contains(&"fmt.Println".to_string()));
    }

    #[test]
    fn extract_imports() {
        let src = b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(imports.contains(&("fmt".to_string(), "fmt".to_string())));
        assert!(imports.contains(&("auth".to_string(), "github.com/foo/auth".to_string())));
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn extract_method_and_type_defs() {
        let src = b"package main\n\ntype Server struct{}\n\nfunc (s *Server) Start() {}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(defs.contains(&"Server".to_string()));
        assert!(defs.contains(&"Start".to_string()));
    }

    // ── Identifier-as-VALUE refs (mache dead_code false-positive fix) ──

    /// Composite literals that pass a function by name in a field-value
    /// position (`cobra.Command{RunE: runServe}`) MUST surface the
    /// function as a `node_refs` entry — pre-fix mache's `dead_code`
    /// rule saw `runServe` as unused because only the call-site
    /// (`function_declaration` for the def) was captured, not the
    /// value-reference from the composite literal.
    #[test]
    fn extract_keyed_element_identifier_as_ref() {
        let src = br#"package main

func runServe() {}

type Cmd struct {
    RunE func()
}

var _ = &Cmd{RunE: runServe}
"#;
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"runServe".to_string()),
            "keyed_element value must emit `runServe` as a ref; got {refs:?}"
        );
    }

    /// Function-call arguments that are bare identifiers MUST surface
    /// as `node_refs` entries — factory-style APIs pass handlers as
    /// values, and mache's `dead_code` rule needs the reference.
    #[test]
    fn extract_argument_list_identifier_as_ref() {
        let src = br#"package main

func handler() {}
func register(f func()) {}

func main() {
    register(handler)
}
"#;
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"handler".to_string()),
            "argument_list identifier must emit `handler` as a ref; got {refs:?}"
        );
        // Sanity: the call target (`register`) is still captured too.
        assert!(
            refs.contains(&"register".to_string()),
            "call-target `register` must also be a ref; got {refs:?}"
        );
    }

    /// Cross-pattern: multiple value-position refs in the same
    /// composite literal + one in an argument list. Every function
    /// name mache's `dead_code` rule cares about must appear.
    #[test]
    fn extract_mixed_value_position_refs() {
        let src = br#"package main

func runServe() {}
func runPing() {}
func middleware() {}

type Command struct {
    RunE   func()
    PostE  func()
}

func New(m func()) *Command {
    return &Command{RunE: runServe, PostE: runPing}
}

var _ = New(middleware)
"#;
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for expected in ["runServe", "runPing", "middleware", "New"] {
            assert!(
                refs.contains(&expected.to_string()),
                "expected ref `{expected}` missing; got {refs:?}"
            );
        }
    }

    // ── Structural qualifier column (bead ley-line-open-4dde42) ────────

    /// The BARE-token row of a qualified call's dual-emit pair carries
    /// the receiver/selector text in `node_refs.qualifier`; the
    /// QUALIFIED-token row and genuinely bare calls carry NULL. Exactly
    /// one row per qualified call site holds the structural
    /// (name, qualifier) pair, so package-scoped consumer rules
    /// (mache's fatal_call qualifier JOIN _imports.alias) can GROUP
    /// BY/filter without string-splitting tokens.
    #[test]
    fn qualifier_column_bare_row_of_dual_emit() {
        let src =
            b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n\tAdd()\n}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("Println".to_string(), Some("fmt".to_string()))),
            "bare row of fmt.Println must carry qualifier 'fmt'; got {refs:?}"
        );
        assert!(
            refs.contains(&("fmt.Println".to_string(), None)),
            "qualified row must carry NULL qualifier (its token embeds it); got {refs:?}"
        );
        assert!(
            refs.contains(&("Add".to_string(), None)),
            "bare call must carry NULL qualifier; got {refs:?}"
        );
    }

    /// Chained selectors: the qualifier is the FULL operand text, same
    /// text the dual-emit joins into the qualified token.
    #[test]
    fn qualifier_column_chained_selector_operand() {
        let src = b"package main\n\nfunc f(a A) {\n\ta.b.Func()\n}\n";
        let (conn, tree) = parse_go(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("Func".to_string(), Some("a.b".to_string()))),
            "chained selector must carry the full operand 'a.b'; got {refs:?}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "java")]
mod java_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_java(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    // Dispatches through `extract_refs` (not a direct extractor call) so
    // the test also gates the factory arm — a query-native language with
    // no dispatch arm silently extracts nothing.
    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_refs(
                        &child,
                        src,
                        &id,
                        "Test.java",
                        crate::languages::TsLanguage::Java,
                        None,
                    );
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_type_defs() {
        // All four Java type-declaration shapes emit defs.
        let src = b"class Server {}\ninterface Handler {}\nenum Color { RED }\nrecord Point(int x, int y) {}\n";
        let (conn, tree) = parse_java(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in ["Server", "Handler", "Color", "Point"] {
            assert!(
                defs.contains(&want.to_string()),
                "missing type def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_method_defs_are_qualified() {
        // Methods dual-emit `Type.method` + `method` across all the
        // bodies that can hold a method_declaration: class_body,
        // interface_body, and enum_body_declarations.
        let src = b"class Server { void validate() {} }\ninterface Handler { void handle(); }\nenum Color { RED; void paint() {} }\n";
        let (conn, tree) = parse_java(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in [
            "Server.validate",
            "validate",
            "Handler.handle",
            "handle",
            "Color.paint",
            "paint",
        ] {
            assert!(
                defs.contains(&want.to_string()),
                "missing method def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_call_refs_dual_emit_receiver() {
        // Bare invocation emits the bare name; receiver invocations
        // dual-emit `receiver.method` + `method` — the receiver is the
        // `object` field of ANY kind, so `this.cfg.batch()` emits
        // `this.cfg.batch` + `batch`. Constructor calls (`new Point`)
        // are the call-sites for classes; mache's dead_code rule needs
        // the ref.
        let src =
            b"class A { void m() { helper(); obj.run(); this.cfg.batch(); new Point(1, 2); } }";
        let (conn, tree) = parse_java(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for want in [
            "helper",
            "run",
            "obj.run",
            "batch",
            "this.cfg.batch",
            "Point",
        ] {
            assert!(
                refs.contains(&want.to_string()),
                "missing call ref {want:?}: {refs:?}"
            );
        }
    }

    #[test]
    fn extract_imports_skip_wildcards() {
        // Scoped imports alias to the last `.` segment; static imports
        // ride the same scoped_identifier shape; a bare `import foo;`
        // is its own alias. `import java.util.*;` matches nothing —
        // a wildcard has no addressable alias (same rule as Rust's
        // `use foo::*`).
        let src = b"import java.util.List;\nimport static java.lang.Math.max;\nimport foo;\nimport java.util.*;\nclass A {}\n";
        let (conn, tree) = parse_java(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&("List".to_string(), "java.util.List".to_string())),
            "missing scoped import: {imports:?}"
        );
        assert!(
            imports.contains(&("max".to_string(), "java.lang.Math.max".to_string())),
            "missing static import: {imports:?}"
        );
        assert!(
            imports.contains(&("foo".to_string(), "foo".to_string())),
            "missing bare import: {imports:?}"
        );
        assert_eq!(
            imports.len(),
            3,
            "wildcard import must not emit: {imports:?}"
        );
    }

    /// Structural qualifier column (bead `ley-line-open-4dde42`): the
    /// bare-token row of a receiver invocation carries the receiver
    /// text; the qualified row and receiverless calls carry NULL.
    #[test]
    fn qualifier_column_on_receiver_invocations() {
        let src = b"class A { void f(Helper h) { h.work(); go(); } }";
        let (conn, tree) = parse_java(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("work".to_string(), Some("h".to_string()))),
            "bare row of h.work() must carry qualifier 'h'; got {refs:?}"
        );
        assert!(
            refs.contains(&("h.work".to_string(), None)),
            "qualified row must carry NULL qualifier; got {refs:?}"
        );
        assert!(
            refs.contains(&("go".to_string(), None)),
            "receiverless call must carry NULL qualifier; got {refs:?}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "c")]
mod c_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_c(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    // Dispatches through `extract_refs` — see java_tests::walk_and_insert.
    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_refs(
                        &child,
                        src,
                        &id,
                        "test.c",
                        crate::languages::TsLanguage::C,
                        None,
                    );
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_function_defs_and_prototypes() {
        // Anchoring at `function_declarator` covers definitions AND
        // prototypes (declaration-only), and a pointer-returning
        // definition whose function_declarator nests inside a
        // pointer_declarator.
        let src =
            b"int add(int a, int b);\nint add(int a, int b) { return a + b; }\nint *alloc(void) { return 0; }\n";
        let (conn, tree) = parse_c(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(defs.contains(&"add".to_string()), "missing add: {defs:?}");
        assert!(
            defs.contains(&"alloc".to_string()),
            "missing pointer-return alloc: {defs:?}"
        );
    }

    #[test]
    fn extract_type_defs() {
        // struct/union/enum specifiers with a BODY are defs; a bodyless
        // `struct Node` usage inside the typedef emits only through the
        // type_definition anchor.
        let src = b"struct Node { int v; };\nunion U { int i; };\nenum Color { RED };\ntypedef struct Node Node;\ntypedef unsigned long size_type;\n";
        let (conn, tree) = parse_c(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in ["Node", "U", "Color", "size_type"] {
            assert!(
                defs.contains(&want.to_string()),
                "missing type def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_call_refs() {
        // Bare calls emit the identifier; function-pointer calls through
        // `.` / `->` emit the bare field name (the receiver is a value,
        // not a ref — same rule as Rust method calls).
        let src = b"void f(void) { g(); s.handle(); p->cb(1); }";
        let (conn, tree) = parse_c(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for want in ["g", "handle", "cb"] {
            assert!(
                refs.contains(&want.to_string()),
                "missing call ref {want:?}: {refs:?}"
            );
        }
    }

    #[test]
    fn extract_includes_strip_quotes_and_angle_brackets() {
        // System includes carry `<...>` in the node text; local includes
        // carry `"..."`. Both delimiters strip; the alias defaults to
        // the path's last `/` segment (engine rule), so `<sys/types.h>`
        // aliases as `types.h`.
        let src = b"#include <stdio.h>\n#include <sys/types.h>\n#include \"local.h\"\n";
        let (conn, tree) = parse_c(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&("stdio.h".to_string(), "stdio.h".to_string())),
            "missing system include: {imports:?}"
        );
        assert!(
            imports.contains(&("types.h".to_string(), "sys/types.h".to_string())),
            "missing nested system include: {imports:?}"
        );
        assert!(
            imports.contains(&("local.h".to_string(), "local.h".to_string())),
            "missing quoted include: {imports:?}"
        );
        assert_eq!(imports.len(), 3, "unexpected extra imports: {imports:?}");
    }
}

#[cfg(test)]
#[cfg(feature = "cpp")]
mod cpp_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_cpp(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    // Dispatches through `extract_refs` — see java_tests::walk_and_insert.
    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_refs(
                        &child,
                        src,
                        &id,
                        "test.cpp",
                        crate::languages::TsLanguage::Cpp,
                        None,
                    );
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_class_namespace_and_inclass_method_defs() {
        // Class + struct + namespace defs, and in-class methods
        // (declaration `area();` AND inline definition `draw() {}`)
        // dual-emit `Class::method` + `method`.
        let src = b"namespace geo {\nclass Shape { public: double area(); void draw() {} };\nstruct Box { int v; };\n}\n";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in [
            "geo",
            "Shape",
            "Box",
            "Shape::area",
            "area",
            "Shape::draw",
            "draw",
        ] {
            assert!(
                defs.contains(&want.to_string()),
                "missing def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_out_of_line_method_defs_qualified() {
        // `double Shape::area() {}` — the qualified_identifier is a
        // DOWNWARD child of the function_declarator, so the dual-emit
        // (`Shape::area` + `area`, separator `::`) is pure query data.
        let src = b"class Shape { public: double area(); };\ndouble Shape::area() { return 0; }\n";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(
            defs.contains(&"Shape::area".to_string()),
            "missing qualified out-of-line def: {defs:?}"
        );
        assert!(
            defs.contains(&"area".to_string()),
            "missing bare out-of-line def: {defs:?}"
        );
    }

    #[test]
    fn extract_template_function_defs() {
        // A template function's function_declarator parses identically
        // to a plain function's — the template_declaration wrapper is
        // transparent to the anchored pattern.
        let src = b"template <typename T> T maxi(T a, T b) { return a; }\n";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(
            defs.contains(&"maxi".to_string()),
            "missing template function def: {defs:?}"
        );
    }

    #[test]
    fn extract_call_refs_bare_method_and_qualified() {
        // Bare calls, method calls via `.` / `->` (bare field name),
        // and namespace-qualified calls dual-emitting `geo::sync` +
        // `sync` on the `::` separator.
        let src = b"void f() { render(); obj.draw(); ptr->flush(); geo::sync(); }";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for want in ["render", "draw", "flush", "geo::sync", "sync"] {
            assert!(
                refs.contains(&want.to_string()),
                "missing call ref {want:?}: {refs:?}"
            );
        }
    }

    #[test]
    fn extract_includes_strip_quotes_and_angle_brackets() {
        // Same include algebra as C — one preproc_include pattern.
        let src = b"#include <vector>\n#include \"widget.hpp\"\n";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&("vector".to_string(), "vector".to_string())),
            "missing system include: {imports:?}"
        );
        assert!(
            imports.contains(&("widget.hpp".to_string(), "widget.hpp".to_string())),
            "missing quoted include: {imports:?}"
        );
    }

    /// Structural qualifier column (bead `ley-line-open-4dde42`) on the
    /// `::` separator: the bare-token row of `geo::sync()` carries
    /// qualifier 'geo'; the qualified row and bare calls carry NULL.
    #[test]
    fn qualifier_column_on_namespace_qualified_calls() {
        let src = b"void f() { geo::sync(); render(); }";
        let (conn, tree) = parse_cpp(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("sync".to_string(), Some("geo".to_string()))),
            "bare row of geo::sync() must carry qualifier 'geo'; got {refs:?}"
        );
        assert!(
            refs.contains(&("geo::sync".to_string(), None)),
            "qualified row must carry NULL qualifier; got {refs:?}"
        );
        assert!(
            refs.contains(&("render".to_string(), None)),
            "bare call must carry NULL qualifier; got {refs:?}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "rust")]
mod rust_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_rust(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_rust(&child, src, &id, "test.rs", None);
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_function_and_method_defs() {
        let src =
            b"fn add() {}\n\nfn sub() {}\n\nstruct Server;\n\nimpl Server { fn start(&self) {} }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        // Bare functions + impl method + struct.
        assert!(defs.contains(&"add".to_string()), "missing add: {defs:?}");
        assert!(defs.contains(&"sub".to_string()), "missing sub: {defs:?}");
        assert!(
            defs.contains(&"start".to_string()),
            "missing start: {defs:?}"
        );
        assert!(
            defs.contains(&"Server".to_string()),
            "missing Server: {defs:?}"
        );
    }

    #[test]
    fn extract_type_kind_defs() {
        let src = b"struct S;\nenum E { A, B }\ntrait T { fn x(&self); }\ntype Alias = u32;\nconst K: u32 = 1;\nstatic S2: u32 = 2;\nmod m { fn inner() {} }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in &["S", "E", "T", "Alias", "K", "S2", "m", "x", "inner"] {
            assert!(defs.contains(&want.to_string()), "missing {want}: {defs:?}");
        }
    }

    #[test]
    fn extract_call_refs_bare_and_method_and_scoped() {
        let src = b"fn main() { foo(); obj.bar(); std::process::exit(0); }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        // Bare call.
        assert!(refs.contains(&"foo".to_string()), "missing foo: {refs:?}");
        // Method call (field_expression's `field`).
        assert!(refs.contains(&"bar".to_string()), "missing bar: {refs:?}");
        // Scoped call: both the qualified and bare forms.
        assert!(
            refs.contains(&"exit".to_string()),
            "missing bare exit: {refs:?}"
        );
        assert!(
            refs.contains(&"std::process::exit".to_string()),
            "missing qualified: {refs:?}"
        );
    }

    #[test]
    fn extract_macro_invocations() {
        let src = b"fn main() { println!(\"hi\"); vec![1,2,3]; std::format!(\"{}\", 1); }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"println".to_string()),
            "missing println: {refs:?}"
        );
        assert!(refs.contains(&"vec".to_string()), "missing vec: {refs:?}");
        assert!(
            refs.contains(&"format".to_string()),
            "missing format: {refs:?}"
        );
    }

    #[test]
    fn extract_use_bare_scoped_and_alias() {
        let src = b"use foo;\nuse std::collections::HashMap;\nuse std::io as io_mod;\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        // Bare.
        assert!(
            imports.contains(&("foo".to_string(), "foo".to_string())),
            "missing bare: {imports:?}"
        );
        // Scoped — alias is last segment.
        assert!(
            imports.contains(&(
                "HashMap".to_string(),
                "std::collections::HashMap".to_string()
            )),
            "missing scoped: {imports:?}"
        );
        // Aliased.
        assert!(
            imports.contains(&("io_mod".to_string(), "std::io".to_string())),
            "missing alias: {imports:?}"
        );
    }

    #[test]
    fn extract_use_list_expands_each_leaf() {
        let src = b"use std::collections::{HashMap, HashSet, BTreeMap as Tree};\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&(
                "HashMap".to_string(),
                "std::collections::HashMap".to_string()
            )),
            "missing HashMap from list: {imports:?}"
        );
        assert!(
            imports.contains(&(
                "HashSet".to_string(),
                "std::collections::HashSet".to_string()
            )),
            "missing HashSet from list: {imports:?}"
        );
        assert!(
            imports.contains(&("Tree".to_string(), "std::collections::BTreeMap".to_string())),
            "missing aliased BTreeMap from list: {imports:?}"
        );
    }

    #[test]
    fn extract_skips_wildcard_use() {
        let src = b"use foo::*;\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        // Wildcards have no addressable alias — extractor must drop them.
        assert!(
            imports.is_empty(),
            "wildcard must not produce import: {imports:?}"
        );
    }

    /// Structural qualifier column (bead `ley-line-open-4dde42`) on the
    /// `::` separator: the bare-token row of a scoped call carries the
    /// FULL path text ('std::process' for std::process::exit); the
    /// qualified row and bare calls carry NULL.
    #[test]
    fn qualifier_column_on_scoped_calls() {
        let src = b"fn main() { std::process::exit(0); helper(); }\n";
        let (conn, tree) = parse_rust(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("exit".to_string(), Some("std::process".to_string()))),
            "bare row of std::process::exit() must carry qualifier 'std::process'; got {refs:?}"
        );
        assert!(
            refs.contains(&("std::process::exit".to_string(), None)),
            "qualified row must carry NULL qualifier; got {refs:?}"
        );
        assert!(
            refs.contains(&("helper".to_string(), None)),
            "bare call must carry NULL qualifier; got {refs:?}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "sql")]
mod sql_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_sql(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    // Dispatches through `extract_refs` — see java_tests::walk_and_insert.
    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_refs(
                        &child,
                        src,
                        &id,
                        "schema.sql",
                        crate::languages::TsLanguage::Sql,
                        None,
                    );
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_ddl_name_defs() {
        // The five def-bearing DDL shapes: table (incl. TEMPORARY),
        // view, materialized view, function (incl. OR REPLACE), schema.
        let src = b"CREATE TABLE users (id INT);\n\
            CREATE TEMPORARY TABLE tmp1 (x INT);\n\
            CREATE VIEW active AS SELECT * FROM users;\n\
            CREATE MATERIALIZED VIEW mv AS SELECT * FROM users;\n\
            CREATE OR REPLACE FUNCTION add_one(x INT) RETURNS INT AS $$ SELECT x + 1 $$ LANGUAGE sql;\n\
            CREATE SCHEMA analytics;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in ["users", "tmp1", "active", "mv", "add_one", "analytics"] {
            assert!(
                defs.contains(&want.to_string()),
                "missing DDL def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_schema_qualified_defs_dual_emit() {
        // `CREATE TABLE analytics.events` — object_reference carries a
        // schema field; the engine dual-emits `analytics.events` +
        // `events` (qualified first).
        let src = b"CREATE TABLE analytics.events (id INT);\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(
            defs.contains(&"analytics.events".to_string()),
            "missing qualified def: {defs:?}"
        );
        assert!(
            defs.contains(&"events".to_string()),
            "missing bare def: {defs:?}"
        );
    }

    #[test]
    fn extract_relation_refs_from_join_update_insert_delete() {
        // Every use-site shape joins back to a CREATE TABLE def token:
        // FROM + JOIN (relation), UPDATE (relation), INSERT INTO
        // (object_reference directly under insert), DELETE FROM (from
        // holds object_reference directly — no relation wrapper).
        let src = b"SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id;\n\
            UPDATE accounts SET v = 1;\n\
            INSERT INTO events (id) VALUES (1);\n\
            DELETE FROM sessions WHERE id = 2;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for want in ["users", "orders", "accounts", "events", "sessions"] {
            assert!(
                refs.contains(&want.to_string()),
                "missing relation ref {want:?}: {refs:?}"
            );
        }
        // Column tokens must NOT emit — bare column names collide across
        // tables (schema resolution is out of the token algebra).
        for junk in ["name", "user_id", "v"] {
            assert!(
                !refs.contains(&junk.to_string()),
                "column token {junk:?} must not emit as a ref: {refs:?}"
            );
        }
    }

    #[test]
    fn extract_schema_qualified_relation_refs_dual_emit() {
        let src = b"SELECT * FROM analytics.events;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"analytics.events".to_string()),
            "missing qualified relation ref: {refs:?}"
        );
        assert!(
            refs.contains(&"events".to_string()),
            "missing bare relation ref: {refs:?}"
        );
    }

    #[test]
    fn extract_invocation_refs() {
        // Function call sites are the join partners of CREATE FUNCTION
        // defs. Builtins (count) emit as unresolved refs — same class
        // as printf in C.
        let src = b"SELECT add_one(2), count(*) FROM users;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"add_one".to_string()),
            "missing invocation ref: {refs:?}"
        );
        assert!(
            refs.contains(&"count".to_string()),
            "missing builtin invocation ref: {refs:?}"
        );
    }

    #[test]
    fn extract_trigger_edges_not_trigger_name() {
        // A trigger's NAME is never referenceable (only DROP TRIGGER) —
        // no def. Its body edges ARE use-sites: the ON table and the
        // EXECUTE FUNCTION target both emit refs, so a function used
        // only by a trigger is not dead.
        let src =
            b"CREATE TRIGGER trg AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION add_one();\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        assert!(
            refs.contains(&"users".to_string()),
            "missing trigger ON-table ref: {refs:?}"
        );
        assert!(
            refs.contains(&"add_one".to_string()),
            "missing trigger EXECUTE FUNCTION ref: {refs:?}"
        );
        assert!(
            !refs.contains(&"trg".to_string()),
            "trigger name must not emit as a ref: {refs:?}"
        );
        let defs = all_defs(&conn);
        assert!(
            !defs.contains(&"trg".to_string()),
            "trigger name must not emit as a def (no use-site exists): {defs:?}"
        );
    }

    #[test]
    fn rejected_emissions_stay_silent() {
        // Index defs (no use-site in the language), DROP/ALTER targets
        // (lifecycle, not use), and CTE names (query-scoped) must not
        // emit. `legacy` appears ONLY in DROP/ALTER position; `recent`
        // only as a CTE name; the CTE's FROM ref (`orders`) still
        // emits via the relation pattern.
        let src = b"CREATE INDEX idx_users_name ON users (name);\n\
            DROP TABLE legacy;\n\
            ALTER TABLE legacy ADD COLUMN email TEXT;\n\
            WITH recent AS (SELECT * FROM orders) SELECT * FROM recent;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(
            !defs.contains(&"idx_users_name".to_string()),
            "index name must not emit as a def: {defs:?}"
        );
        assert!(
            !defs.contains(&"recent".to_string()),
            "CTE name must not emit as a def: {defs:?}"
        );
        let refs = all_refs(&conn);
        assert!(
            !refs.contains(&"legacy".to_string()),
            "DROP/ALTER targets must not emit as refs: {refs:?}"
        );
        // CREATE INDEX's ON-table object_reference is also lifecycle
        // metadata, not use — `users` here appears only in the index
        // statement, so it must not ref.
        assert!(
            !refs.contains(&"users".to_string()),
            "CREATE INDEX ON-table must not emit as a ref: {refs:?}"
        );
        assert!(
            refs.contains(&"orders".to_string()),
            "CTE body FROM ref must still emit: {refs:?}"
        );
    }

    #[test]
    fn sql_has_no_import_algebra() {
        // SQL has no in-language import construct (\i and \include are
        // psql metacommands outside the grammar) — _imports stays empty.
        let src = b"CREATE TABLE users (id INT);\nSELECT * FROM users;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(imports.is_empty(), "sql must emit no imports: {imports:?}");
    }

    /// Structural qualifier column (bead `ley-line-open-4dde42`) on the
    /// schema-dot separator: the bare-token row of a schema-qualified
    /// relation ref carries the schema name; the qualified row and
    /// unqualified relations carry NULL.
    #[test]
    fn qualifier_column_on_schema_qualified_relations() {
        let src = b"SELECT id FROM analytics.events;\nSELECT id FROM users;\n";
        let (conn, tree) = parse_sql(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = refs_with_qualifier(&conn);
        assert!(
            refs.contains(&("events".to_string(), Some("analytics".to_string()))),
            "bare row of analytics.events must carry qualifier 'analytics'; got {refs:?}"
        );
        assert!(
            refs.contains(&("analytics.events".to_string(), None)),
            "qualified row must carry NULL qualifier; got {refs:?}"
        );
        assert!(
            refs.contains(&("users".to_string(), None)),
            "unqualified relation must carry NULL qualifier; got {refs:?}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "bash")]
mod bash_tests {
    use super::*;
    use crate::schema::{create_ast_schema, create_refs_schema};
    use rusqlite::Connection;
    use tree_sitter::Parser;

    fn parse_bash(src: &[u8]) -> (Connection, tree_sitter::Tree) {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (conn, tree)
    }

    // Dispatches through `extract_refs` — see java_tests::walk_and_insert.
    fn walk_and_insert(node: tree_sitter::Node, src: &[u8], conn: &Connection, prefix: &str) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.is_named() {
                    let id = format!("{prefix}/{}", child.kind());
                    let refs = extract_refs(
                        &child,
                        src,
                        &id,
                        "script.sh",
                        crate::languages::TsLanguage::Bash,
                        None,
                    );
                    insert_extracted_refs(conn, &refs).unwrap();
                    walk_and_insert(child, src, conn, &id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn all_defs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_defs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_refs(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT token FROM node_refs ORDER BY token")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn all_imports(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT alias, path FROM _imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn extract_function_defs_both_spellings() {
        // POSIX `f() {}` and bash `function f {}` are both
        // function_definition nodes with a `word` name field.
        let src = b"my_func() {\n  echo hi\n}\nfunction other_func {\n  my_func\n}\n";
        let (conn, tree) = parse_bash(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        for want in ["my_func", "other_func"] {
            assert!(
                defs.contains(&want.to_string()),
                "missing function def {want:?}: {defs:?}"
            );
        }
    }

    #[test]
    fn extract_command_refs_static_names_only() {
        // Statically-named commands ref (joins to shell-function defs;
        // externals like grep emit unresolved — same class as printf in
        // C). Commands invoked through an expansion have no stable
        // token and must not emit. Command substitution bodies are real
        // command nodes — `$(my_func)` refs.
        let src = b"my_func() { :; }\nmy_func\ngrep -r foo .\nresult=$(my_func)\n\"$CMD\" --flag\n";
        let (conn, tree) = parse_bash(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let refs = all_refs(&conn);
        for want in ["my_func", "grep"] {
            assert!(
                refs.contains(&want.to_string()),
                "missing command ref {want:?}: {refs:?}"
            );
        }
        assert!(
            !refs.iter().any(|r| r.contains("CMD")),
            "expansion-invoked command must not emit a ref: {refs:?}"
        );
    }

    #[test]
    fn extract_source_imports_static_paths() {
        // `source` and `.` with a static word path import; a static
        // double-quoted string path imports quote-stripped; the alias
        // defaults to the path's last `/` segment (engine rule).
        let src = b"source ./lib/common.sh\n. /etc/profile.d/vars.sh\nsource \"config/local.sh\"\n";
        let (conn, tree) = parse_bash(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.contains(&("common.sh".to_string(), "./lib/common.sh".to_string())),
            "missing word-path source import: {imports:?}"
        );
        assert!(
            imports.contains(&("vars.sh".to_string(), "/etc/profile.d/vars.sh".to_string())),
            "missing dot-command import: {imports:?}"
        );
        assert!(
            imports.contains(&("local.sh".to_string(), "config/local.sh".to_string())),
            "missing string-path source import: {imports:?}"
        );
        assert_eq!(imports.len(), 3, "exactly three imports: {imports:?}");
        // One node, one fact: a source command emits its Import, never
        // a `source` / `.` command ref alongside.
        let refs = all_refs(&conn);
        assert!(
            !refs.contains(&"source".to_string()) && !refs.contains(&".".to_string()),
            "source/. must not double-emit as command refs: {refs:?}"
        );
    }

    #[test]
    fn dynamic_source_paths_do_not_import() {
        // An expansion-carrying path is not statically resolvable — the
        // sole-named-child string anchor excludes it, and the bare-word
        // pattern never matches a `string` argument.
        let src = b"source \"$HOME/.config/thing.sh\"\nsource $DYNAMIC\n";
        let (conn, tree) = parse_bash(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let imports = all_imports(&conn);
        assert!(
            imports.is_empty(),
            "dynamic source paths must not import: {imports:?}"
        );
    }

    #[test]
    fn variables_and_expansions_stay_silent() {
        // Variable assignments are not defs and expansions are not refs
        // BY DESIGN: dynamic scoping + export/env crossing file
        // boundaries makes the def↔ref join unsound, and expansion refs
        // are the noisiest emission shell has.
        let src =
            b"VAR=value\nexport PATH=\"$PATH:/usr/local/bin\"\nlocal x=1\necho \"$VAR\" $PATH\n";
        let (conn, tree) = parse_bash(src);
        walk_and_insert(tree.root_node(), src, &conn, "");
        let defs = all_defs(&conn);
        assert!(
            defs.is_empty(),
            "variable assignments must not emit defs: {defs:?}"
        );
        let refs = all_refs(&conn);
        for junk in ["VAR", "PATH", "x"] {
            assert!(
                !refs.contains(&junk.to_string()),
                "variable expansion {junk:?} must not emit a ref: {refs:?}"
            );
        }
        // The echo command itself still refs (it is a command).
        assert!(
            refs.contains(&"echo".to_string()),
            "echo command ref must emit: {refs:?}"
        );
    }
}
