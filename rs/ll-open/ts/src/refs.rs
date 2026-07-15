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
pub const EXTRACTION_EPOCH: u64 = 1;

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
    Ref {
        token: String,
        node_id: String,
        source_id: String,
        container_node_id: Option<String>,
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
            } => crate::schema::insert_ref(
                conn,
                token,
                node_id,
                source_id,
                container_node_id.as_deref(),
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
        _ => Vec::new(),
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
/// Node kinds handled (tree-sitter-rust grammar):
/// - `function_item`, `struct_item`, `enum_item`, `union_item`,
///   `trait_item`, `type_item`, `mod_item`, `const_item`, `static_item`
///   → Def (uses `name` field)
/// - `call_expression`:
///     - `function: identifier`           → Ref (bare token)
///     - `function: field_expression`     → Ref (method name from `field`)
///     - `function: scoped_identifier`    → Ref (qualified `pkg::func` + bare `func`)
/// - `macro_invocation` → Ref (`macro` field; includes the `!` is dropped)
/// - `use_declaration` → Import. Handles bare, scoped, aliased, and
///   list/scoped-list use trees. Wildcards are skipped (no addressable
///   alias). Nested `use_list` cases recurse via the walker, not here.
///
/// Closures (`closure_expression`) are intentionally NOT matched — they're
/// anonymous, no stable token.
#[cfg(feature = "rust")]
pub fn extract_rust(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    let mut out = Vec::new();

    match node.kind() {
        // ── Definitions: anything with a `name` field that introduces a
        // top-level binding the rest of the codebase can reference.
        // `function_signature_item` is the bodyless form used in traits
        // (`fn x(&self);`) and `extern` blocks. Same `name` field.
        "function_item" | "function_signature_item" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(source)
                && !name.is_empty()
            {
                // Bead `ley-line-open-caf423`: when this `function_item`
                // lives inside an `impl` block, emit the qualified
                // `Receiver::method` form alongside the bare method name
                // so mache's cross-language rules can disambiguate
                // methods on different types. Walk parent chain:
                //   function_item → declaration_list → impl_item(type=...)
                // Trait signatures (`function_signature_item`) inside a
                // `trait_item` get the same treatment via the same
                // pattern — trait_item also carries a `name` field.
                if let Some(recv) = rust_impl_receiver(node, source) {
                    out.push(ExtractedRef::Def {
                        token: format!("{recv}::{name}"),
                        node_id: node_id.to_string(),
                        source_id: source_id.to_string(),
                        container_node_id: container_node_id.map(str::to_string),
                        canonical_kind: crate::languages::TsLanguage::Rust
                            .canonical_kind(node.kind()),
                    });
                }
                out.push(ExtractedRef::Def {
                    token: name.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind: crate::languages::TsLanguage::Rust.canonical_kind(node.kind()),
                });
            }
        }
        "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" | "mod_item"
        | "const_item" | "static_item" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(token) = name_node.utf8_text(source)
                && !token.is_empty()
            {
                out.push(ExtractedRef::Def {
                    token: token.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind: crate::languages::TsLanguage::Rust.canonical_kind(node.kind()),
                });
            }
        }

        // ── Call references: tree-sitter-rust's `call_expression` always
        // has a `function` field; we branch on that field's kind.
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    // Bare call: `foo()`.
                    "identifier" => {
                        if let Ok(token) = func_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                                container_node_id: container_node_id.map(str::to_string),
                            });
                        }
                    }
                    // Method-like: `obj.method()`. The receiver isn't a
                    // ref (it's a value), so we emit only the field name.
                    "field_expression" => {
                        if let Some(field_node) = func_node.child_by_field_name("field")
                            && let Ok(token) = field_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                                container_node_id: container_node_id.map(str::to_string),
                            });
                        }
                    }
                    // Qualified: `mod::func()`. Emit both the qualified
                    // form ("module::func") and the bare form ("func") so
                    // a downstream resolver can match either.
                    "scoped_identifier" => {
                        let qualified = func_node.utf8_text(source).unwrap_or("");
                        let bare = func_node
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if !bare.is_empty() {
                            if !qualified.is_empty() {
                                out.push(ExtractedRef::Ref {
                                    token: qualified.to_string(),
                                    node_id: node_id.to_string(),
                                    source_id: source_id.to_string(),
                                    container_node_id: container_node_id.map(str::to_string),
                                });
                            }
                            out.push(ExtractedRef::Ref {
                                token: bare.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                                container_node_id: container_node_id.map(str::to_string),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // ── Macro invocations: `println!`, `vec!`, etc.
        "macro_invocation" => {
            if let Some(macro_node) = node.child_by_field_name("macro") {
                // `macro` may be `identifier` or `scoped_identifier`.
                let token = match macro_node.kind() {
                    "scoped_identifier" => macro_node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or(""),
                    _ => macro_node.utf8_text(source).unwrap_or(""),
                };
                if !token.is_empty() {
                    out.push(ExtractedRef::Ref {
                        token: token.to_string(),
                        node_id: node_id.to_string(),
                        source_id: source_id.to_string(),
                        container_node_id: container_node_id.map(str::to_string),
                    });
                }
            }
        }

        // ── Imports: `use_declaration` wraps a single `argument` tree.
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                collect_use_imports(arg, source, source_id, &mut out);
            }
        }

        _ => {}
    }

    out
}

/// Recursively flatten a `use` argument into `ExtractedRef::Import`
/// entries. Tree-sitter-rust models the `use` tree as nested
/// `scoped_identifier` / `use_as_clause` / `use_list` / `scoped_use_list`
/// nodes; the recursion mirrors that shape.
///
/// Wildcards (`use foo::*;`) are skipped — no stable alias to attach.
#[cfg(feature = "rust")]
fn collect_use_imports(
    node: Node<'_>,
    source: &[u8],
    source_id: &str,
    out: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        // Bare `use foo;`
        "identifier" => {
            if let Ok(name) = node.utf8_text(source)
                && !name.is_empty()
            {
                out.push(ExtractedRef::Import {
                    alias: name.to_string(),
                    path: name.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        // `use foo::bar;` — full path is the node text, alias = last
        // segment.
        "scoped_identifier" => {
            let path = node.utf8_text(source).unwrap_or("");
            let alias = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if !path.is_empty() && !alias.is_empty() {
                out.push(ExtractedRef::Import {
                    alias: alias.to_string(),
                    path: path.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
        // `use foo::bar as baz;` — explicit alias.
        "use_as_clause" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let alias = node
                .child_by_field_name("alias")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            if !path.is_empty() && !alias.is_empty() {
                out.push(ExtractedRef::Import {
                    alias: alias.to_string(),
                    path: path.to_string(),
                    source_id: source_id.to_string(),
                });
            }
        }
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
        // `use foo::*;` — intentionally skipped (no addressable alias).
        "use_wildcard" => {}
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
/// single AST node. Pure data — no DB access.
///
/// Bead `ley-line-open-caf423`: Python was recognized by the parse
/// pipeline (had a `TsLanguage::Python` variant, a `.py` extension
/// mapping, and produced `_ast` rows) but the language dispatcher's
/// match had no Python arm, so every Python file wrote zero rows to
/// `node_defs` / `node_refs`. Mache's cross-language rules join on
/// those tables, so the emptiness surfaced as false-positive smells.
///
/// Node kinds handled (tree-sitter-python grammar):
/// - `function_definition` → Def (uses `name` field). When nested inside
///   a `class_definition`, we also emit the qualified `Class.method`
///   form so consumers can distinguish methods on different classes.
/// - `class_definition` → Def (name field).
/// - `call` → Ref:
///     - `function: identifier`   → bare `func` token
///     - `function: attribute`    → qualified `obj.attr` + bare `attr`
/// - `import_statement` → Import (each dotted_name / aliased_import).
/// - `import_from_statement` → Import (each imported name, path is the
///   `module` field).
#[cfg(feature = "python")]
pub fn extract_python(
    node: &Node,
    source: &[u8],
    node_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
) -> Vec<ExtractedRef> {
    let mut out = Vec::new();

    match node.kind() {
        "function_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return out;
            };
            let Ok(name) = name_node.utf8_text(source) else {
                return out;
            };
            if name.is_empty() {
                return out;
            }
            // Method disambiguation: function_definition inside
            // class_definition.body (`block`) is a method. Emit the
            // qualified `Class.method` form so mache's cross-language
            // rules can distinguish methods on different classes.
            if let Some(cls) = python_enclosing_class(node, source) {
                out.push(ExtractedRef::Def {
                    token: format!("{cls}.{name}"),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind: crate::languages::TsLanguage::Python
                        .canonical_kind(node.kind()),
                });
            }
            out.push(ExtractedRef::Def {
                token: name.to_string(),
                node_id: node_id.to_string(),
                source_id: source_id.to_string(),
                container_node_id: container_node_id.map(str::to_string),
                canonical_kind: crate::languages::TsLanguage::Python.canonical_kind(node.kind()),
            });
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(token) = name_node.utf8_text(source)
                && !token.is_empty()
            {
                out.push(ExtractedRef::Def {
                    token: token.to_string(),
                    node_id: node_id.to_string(),
                    source_id: source_id.to_string(),
                    container_node_id: container_node_id.map(str::to_string),
                    canonical_kind: crate::languages::TsLanguage::Python
                        .canonical_kind(node.kind()),
                });
            }
        }
        "call" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                match func_node.kind() {
                    "identifier" => {
                        if let Ok(token) = func_node.utf8_text(source)
                            && !token.is_empty()
                        {
                            out.push(ExtractedRef::Ref {
                                token: token.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                                container_node_id: container_node_id.map(str::to_string),
                            });
                        }
                    }
                    "attribute" => {
                        let attr = func_node
                            .child_by_field_name("attribute")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let obj = func_node
                            .child_by_field_name("object")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if !attr.is_empty() {
                            if !obj.is_empty() {
                                out.push(ExtractedRef::Ref {
                                    token: format!("{obj}.{attr}"),
                                    node_id: node_id.to_string(),
                                    source_id: source_id.to_string(),
                                    container_node_id: container_node_id.map(str::to_string),
                                });
                            }
                            out.push(ExtractedRef::Ref {
                                token: attr.to_string(),
                                node_id: node_id.to_string(),
                                source_id: source_id.to_string(),
                                container_node_id: container_node_id.map(str::to_string),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_statement" => {
            // `import a, b as c, x.y` — each name is a child (`dotted_name`
            // or `aliased_import`). Alias = last segment when unaliased.
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "dotted_name" => {
                        let path = child.utf8_text(source).unwrap_or("");
                        if !path.is_empty() {
                            let alias = path.rsplit('.').next().unwrap_or(path);
                            out.push(ExtractedRef::Import {
                                alias: alias.to_string(),
                                path: path.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    "aliased_import" => {
                        let path = child
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        let alias = child
                            .child_by_field_name("alias")
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("");
                        if !path.is_empty() && !alias.is_empty() {
                            out.push(ExtractedRef::Import {
                                alias: alias.to_string(),
                                path: path.to_string(),
                                source_id: source_id.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_from_statement" => {
            // `from mod import a, b as c`. `module_name` field carries the
            // path prefix; every non-module child is an import target.
            let prefix = node
                .child_by_field_name("module_name")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("");
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "dotted_name" {
                    // The module_name itself is a `dotted_name`; skip it
                    // — we handle only the *imported* names here.
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
// Tests
// ---------------------------------------------------------------------------

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
}
