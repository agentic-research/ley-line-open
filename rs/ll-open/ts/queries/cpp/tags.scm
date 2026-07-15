; C++ extraction query — bead ley-line-open-5e21c2.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod cpp_tests`) and
; cli-lib's def_ref_extraction_fidelity_test (C++ arm).
;
; Query-native language: the C++ grammar is a superset of C's, and
; this file is the C query plus the C++-only patterns (class,
; namespace, in-class methods, `::`-qualified defs and calls). One arm
; stays imperative in refs.rs::extract_cpp (leak note there):
; qualified `Class::method` defs for IN-CLASS members read the class
; name from an ANCESTOR class/struct body, and patterns match downward
; only. Out-of-line `Class::method` definitions qualify here as pure
; query data — the qualified_identifier is a child of the declarator.
;
; Destructors (`~Widget`, destructor_name) and operators (`operator+`,
; operator_name) are NOT matched — neither yields a call-resolvable
; identifier token. Single-level qualification only: `A::B::f` nests
; qualified_identifiers with the leaf on the inside, so the anchored
; pattern's `name: (identifier)` doesn't reach it.
;
; PREPROCESSOR LIMITATION: same as C — the tree-sitter parse is taken
; at face value; macro-produced defs are invisible, inactive-`#ifdef`
; defs still emit. Resolving the preprocessor is out of scope BY
; DESIGN.

; ── Definitions ─────────────────────────────────────────────────────

; Free functions and templates (the template_declaration wrapper is
; transparent to an anchored pattern). Function-pointer declarators
; don't match — their inner declarator is a parenthesized_declarator.
(function_declarator
  declarator: (identifier) @name) @def

; In-class members — declaration (`double area();`) and inline
; definition (`void draw() {}`) both name the member with a
; field_identifier. Bare name only; the qualified `Class::method`
; companion is emitted by `cpp_enclosing_class` (see header).
(function_declarator
  declarator: (field_identifier) @name) @def

; Out-of-line member definition: `double Shape::area() {}` dual-emits
; `Shape::area` + `area`. C++ paths join on `::`, not the engine's `.`
; default, so the separator rides on the pattern.
((function_declarator
  declarator: (qualified_identifier
    scope: (namespace_identifier) @qualifier
    name: (identifier) @name)) @def
 (#set! qualifier-separator "::"))

; Class-shaped specifiers with a BODY are definitions; bodyless uses
; (forward declarations, `class Foo x;`) must not emit def rows.
(class_specifier
  name: (type_identifier) @name
  body: (_)) @def

(struct_specifier
  name: (type_identifier) @name
  body: (_)) @def

(union_specifier
  name: (type_identifier) @name
  body: (_)) @def

(enum_specifier
  name: (type_identifier) @name
  body: (_)) @def

(type_definition
  declarator: (type_identifier) @name) @def

; Namespaces are container symbols, κ "module" — same discipline as
; Rust's mod_item.
(namespace_definition
  name: (namespace_identifier) @name) @def

; ── Call-target references ──────────────────────────────────────────

(call_expression
  function: (identifier) @name) @ref

; Method calls through `.` / `->` emit the bare field name — the
; receiver is a value, not a ref (same rule as Rust method calls).
(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @ref

; Qualified call: `geo::sync()` dual-emits `geo::sync` + `sync`.
; Nested `a::b::f()` doesn't match (leaf nests inside `name:`, see
; header) — deeper paths emit nothing rather than a wrong token.
((call_expression
  function: (qualified_identifier
    scope: (namespace_identifier) @qualifier
    name: (identifier) @name)) @ref
 (#set! qualifier-separator "::"))

; ── Imports ─────────────────────────────────────────────────────────

; Same include algebra as C: `<vector>` / `"widget.hpp"` delimiters
; strip via the engine rule; alias defaults to the last `/` segment.
(preproc_include
  path: (_) @path) @import
