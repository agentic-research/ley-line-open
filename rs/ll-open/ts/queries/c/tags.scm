; C extraction query — bead ley-line-open-5e21c2.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod c_tests`) and
; cli-lib's def_ref_extraction_fidelity_test (C arm).
;
; Query-native language: this file IS the extractor — no imperative
; arm exists (extract_c is a pure engine delegate).
;
; PREPROCESSOR LIMITATION: extraction reads the tree-sitter parse at
; face value. Defs produced by macro expansion are invisible; defs
; inside inactive `#ifdef` branches still emit; a symbol defined in
; both branches of an `#if`/`#else` emits twice. Resolving the
; preprocessor is out of scope BY DESIGN.

; ── Definitions ─────────────────────────────────────────────────────

; Anchoring at function_declarator covers definitions AND prototypes
; (`int add(int, int);`) with one pattern, and is transparent to
; declarator nesting — a pointer-returning definition wraps this node
; in a pointer_declarator, but the anchored match fires regardless of
; parent. Function-pointer declarators don't match: their inner
; declarator is a parenthesized_declarator, not a bare identifier.
(function_declarator
  declarator: (identifier) @name) @def

; struct/union/enum specifiers with a BODY are definitions; bodyless
; uses (`struct Node x;`, `typedef struct Node Node;`) are references
; to a definition that lives elsewhere and must not emit def rows.
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

; ── Call-target references ──────────────────────────────────────────

(call_expression
  function: (identifier) @name) @ref

; Function-pointer calls through `.` / `->` emit the bare field name —
; the receiver is a value, not a ref (same rule as Rust method calls).
(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @ref

; ── Identifier-as-VALUE references (bead ley-line-open-a9ff82) ──────
;
; A function named as a value — a function pointer initialised,
; assigned, passed, address-taken, returned, or placed in an
; initializer list — is a use mache's dead_code needs. Each pattern
; anchors at the value-position CONTAINER (queries match downward
; only); the container's grammar kind tells a value ref from a call
; ref in `node_refs.node_kind` / `kind_id`. Plain variables in these
; positions emit too and stay unbound when no def carries the token.
; Declarators and assignment LHS are bindings and do not emit.

(argument_list
  (identifier) @name) @ref

(init_declarator
  value: (identifier) @name) @ref

(assignment_expression
  right: (identifier) @name) @ref

(return_statement
  (identifier) @name) @ref

; `&helper` — address-of only; `*p` is a dereference, not a use of `p`
; as a function value.
(pointer_expression
  operator: "&"
  argument: (identifier) @name) @ref

; `{ .fn = helper }`
(initializer_pair
  value: (identifier) @name) @ref

; `{ helper, other }`
(initializer_list
  (identifier) @name) @ref

; ── Imports ─────────────────────────────────────────────────────────

; `#include <stdio.h>` (system_lib_string, angle brackets in the node
; text) and `#include "local.h"` (string_literal) both ride the
; engine's delimiter-strip rule; the alias defaults to the path's last
; `/` segment, so `<sys/types.h>` aliases as `types.h`.
(preproc_include
  path: (_) @path) @import
