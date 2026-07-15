; TypeScript extraction query — bead ley-line-open-451f77.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in
; rs/ll-open/cli-lib/tests/def_ref_extraction_fidelity_test.rs.
;
; Compiled against the TSX grammar (LANGUAGE_TSX), whose node kinds are
; a superset of tree-sitter-javascript's — every pattern up to the
; "TypeScript-only definitions" section is byte-identical to
; queries/javascript/tags.scm. The two files stay independent data:
; each must compile against ITS grammar, and a shared include
; mechanism does not exist (deliberately — see bead comments).
;
; Two facts are OUTSIDE the anchored-query vocabulary and stay
; imperative in `js_ts_context_fixups` (src/refs.rs): the qualified
; `Class.method` def (needs the ANCESTOR class name; patterns match
; downward only — covers `abstract_class_declaration` too) and
; κ = "function" for var-bound arrows/function expressions (the
; engine derives κ from the anchor node's kind).

; ── Definitions (shared with JavaScript) ────────────────────────────

(function_declaration
  name: (_) @name) @def

(generator_function_declaration
  name: (_) @name) @def

(class_declaration
  name: (_) @name) @def

; Bare method name only — the qualified `Class.method` companion is
; emitted by `js_ts_context_fixups` (see header). `name: (_)` matches
; every name kind the grammar allows. Interface members are
; `method_signature`, not `method_definition` — no def, matching the
; pre-port extractor.
(method_definition
  name: (_) @name) @def

; Var bindings to arrows / function expressions define callables:
; `const foo = () => 1`. Anchored at the DECLARATION (the fold node
; the pre-port extractor emitted from), one match per qualifying
; declarator. `name: (identifier)` excludes destructuring patterns —
; the emitted token must be a single callable identifier. Bead
; `ley-line-open-caf423`.
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def

; ── TypeScript-only definitions ─────────────────────────────────────
; Type-level constructs don't exist at runtime (except enums, which
; also emit a runtime object) but are stable identifiers other code
; names — mache's cross-language rules resolve against them.

(abstract_class_declaration
  name: (_) @name) @def

(interface_declaration
  name: (_) @name) @def

(type_alias_declaration
  name: (_) @name) @def

(enum_declaration
  name: (_) @name) @def

; ── Call-target references (shared with JavaScript) ─────────────────

(call_expression
  function: (identifier) @name) @ref

; Member calls dual-emit `obj.prop` + `prop` (engine @qualifier rule).
; @qualifier is the object expression of ANY kind — `a.b.c()` emits
; `a.b.c` + `c`, `foo().bar()` emits `foo().bar` + `bar`.
(call_expression
  function: (member_expression
    object: (_) @qualifier
    property: (_) @name)) @ref

; ── Imports (shared with JavaScript) ────────────────────────────────
; @path captures the string_fragment INSIDE the source string, so no
; quote stripping is needed regardless of quote style; an empty source
; (`from ""`) has no fragment and emits nothing. `import type { … }`
; carries the same clause/specifier shape and matches identically.
; `import foo = require("m")` has no import_clause and emits nothing,
; matching the pre-port extractor.

; Default import: `import d from "m"`.
(import_statement
  (import_clause
    (identifier) @alias)
  source: (string (string_fragment) @path)) @import

; Namespace import: `import * as ns from "m"`.
(import_statement
  (import_clause
    (namespace_import (identifier) @alias))
  source: (string (string_fragment) @path)) @import

; Named import without `as`: the local binding IS the imported name,
; so it is captured as @alias directly. The `!alias` negation keeps
; this branch disjoint from the aliased one below.
(import_statement
  (import_clause
    (named_imports
      (import_specifier
        !alias
        name: (identifier) @alias)))
  source: (string (string_fragment) @path)) @import

; Named import with `as`: the local binding is the alias.
(import_statement
  (import_clause
    (named_imports
      (import_specifier
        alias: (identifier) @alias)))
  source: (string (string_fragment) @path)) @import
