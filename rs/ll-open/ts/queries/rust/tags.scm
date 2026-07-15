; Rust extraction query — bead ley-line-open-42f2b3.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod rust_tests`).
;
; Two arms stay imperative in `extract_rust` (documented on the bead):
; - qualified `Receiver::method` defs (bead ley-line-open-caf423) read
;   the receiver from an ANCESTOR impl_item / trait_item; patterns
;   match downward only, so no pattern anchored at the function node
;   can capture it.
; - use-tree flattening: `use a::{b, c as d, e::{f}}` joins the shared
;   path prefix onto each leaf and recurses to unbounded depth; `@path`
;   reads a single node's text, so neither the join nor the recursion
;   fits the query→fact vocabulary.

; ── Definitions ─────────────────────────────────────────────────────

; `function_signature_item` is the bodyless form used in traits
; (`fn x(&self);`) and `extern` blocks — same `name` field.
(function_item name: (_) @name) @def
(function_signature_item name: (_) @name) @def
(struct_item name: (_) @name) @def
(enum_item name: (_) @name) @def
(union_item name: (_) @name) @def
(trait_item name: (_) @name) @def
(type_item name: (_) @name) @def
(mod_item name: (_) @name) @def
(const_item name: (_) @name) @def
(static_item name: (_) @name) @def

; ── Call-target references ──────────────────────────────────────────

; Bare call: `foo()`.
(call_expression
  function: (identifier) @name) @ref

; Method call: `obj.method()` — the receiver is a value, not a ref;
; only the field name emits.
(call_expression
  function: (field_expression
    field: (_) @name)) @ref

; Qualified call: `mod::func()` dual-emits `mod::func` + `func`. Rust
; paths join on `::`, not the engine's `.` default, so the separator
; rides on the pattern. `path` is optional in the grammar (`::foo()`);
; without it only the bare form emits.
((call_expression
  function: (scoped_identifier
    path: (_)? @qualifier
    name: (_) @name)) @ref
 (#set! qualifier-separator "::"))

; ── Macro invocations ───────────────────────────────────────────────

; `println!(..)`, `vec![..]`. Scoped macros (`std::format!`) emit the
; bare name only — no qualified form.
(macro_invocation
  macro: (identifier) @name) @ref
(macro_invocation
  macro: (scoped_identifier
    name: (_) @name)) @ref

; ── Imports (single-leaf forms; list forms are imperative) ──────────

; Bare `use foo;` — path and alias are the same node.
(use_declaration
  argument: (identifier) @path @alias) @import

; `use std::collections::HashMap;` — alias is the last `::` segment.
; The engine's alias default splits on `/`, so Rust captures the alias
; explicitly.
(use_declaration
  argument: (scoped_identifier
    name: (_) @alias) @path) @import

; `use std::io as io_mod;`
(use_declaration
  argument: (use_as_clause
    path: (_) @path
    alias: (_) @alias)) @import

; `use foo::*;` matches no pattern — a wildcard has no addressable
; alias.
