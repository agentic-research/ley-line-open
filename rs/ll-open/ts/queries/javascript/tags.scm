; JavaScript extraction query — bead ley-line-open-451f77.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in
; rs/ll-open/cli-lib/tests/def_ref_extraction_fidelity_test.rs.
;
; Two JS facts are OUTSIDE the anchored-query vocabulary and stay
; imperative in `js_ts_context_fixups` (src/refs.rs): the qualified
; `Class.method` def (needs the ANCESTOR class name; patterns match
; downward only) and κ = "function" for var-bound arrows/function
; expressions (the engine derives κ from the anchor node's kind).

; ── Definitions ─────────────────────────────────────────────────────

(function_declaration
  name: (_) @name) @def

(generator_function_declaration
  name: (_) @name) @def

(class_declaration
  name: (_) @name) @def

; Bare method name only — the qualified `Class.method` companion is
; emitted by `js_ts_context_fixups` (see header). `name: (_)` matches
; every name kind the grammar allows (property_identifier, private
; `#name`, computed, string, number), same reach as the old
; field-text extraction.
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

; ── Call-target references ──────────────────────────────────────────

(call_expression
  function: (identifier) @name) @ref

; Member calls dual-emit `obj.prop` + `prop` (engine @qualifier rule).
; @qualifier is the object expression of ANY kind — `a.b.c()` emits
; `a.b.c` + `c`, `foo().bar()` emits `foo().bar` + `bar`.
(call_expression
  function: (member_expression
    object: (_) @qualifier
    property: (_) @name)) @ref

; ── Identifier-as-VALUE references (bead ley-line-open-a9ff82) ──────
;
; A function named as a value — `const h = helper`, `x = helper`,
; `register(helper)`, `{k: helper}`, `{helper}`, `[helper]`,
; `return helper`, `onClick={helper}`, `export default helper`, or a
; method value `this.handle` — is a use mache's dead_code needs. Each
; pattern anchors at the value-position CONTAINER (queries match
; downward only); the container's grammar kind tells a value ref from
; a call ref in `node_refs.node_kind` / `kind_id`. A member expression
; dual-emits `obj.prop` + `prop` with qualifier `obj`, like a member
; call; property READS in these positions emit too (syntactically
; identical to a method value) and stay unbound when no def carries the
; token. Binding positions (declarator names, assignment LHS) do not
; emit.

(arguments [
  (identifier) @name
  (member_expression object: (_) @qualifier property: (_) @name)
]) @ref

(variable_declarator
  value: [
    (identifier) @name
    (member_expression object: (_) @qualifier property: (_) @name)
  ]) @ref

(assignment_expression
  right: [
    (identifier) @name
    (member_expression object: (_) @qualifier property: (_) @name)
  ]) @ref

(return_statement [
  (identifier) @name
  (member_expression object: (_) @qualifier property: (_) @name)
]) @ref

(pair
  value: [
    (identifier) @name
    (member_expression object: (_) @qualifier property: (_) @name)
  ]) @ref

; Shorthand property `{helper}` names the binding `helper` as a value.
(object
  (shorthand_property_identifier) @name) @ref

(array [
  (identifier) @name
  (member_expression object: (_) @qualifier property: (_) @name)
]) @ref

(jsx_expression [
  (identifier) @name
  (member_expression object: (_) @qualifier property: (_) @name)
]) @ref

(export_statement
  value: [
    (identifier) @name
    (member_expression object: (_) @qualifier property: (_) @name)
  ]) @ref

; ── Imports ─────────────────────────────────────────────────────────
; @path captures the string_fragment INSIDE the source string, so no
; quote stripping is needed regardless of quote style; an empty source
; (`from ""`) has no fragment and emits nothing. Side-effect imports
; (`import "m";`) bind nothing and emit nothing.

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
