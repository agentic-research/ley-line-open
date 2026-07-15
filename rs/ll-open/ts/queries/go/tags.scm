; Go extraction query — bead ley-line-open-206d53.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod tests`).

; ── Definitions ─────────────────────────────────────────────────────

(function_declaration
  name: (identifier) @name) @def

; Method defs dual-emit `Receiver.Method` + `Method` (bead
; ley-line-open-caf423): @qualifier is the receiver TYPE with a
; pointer receiver's `*` stripped. The three alternation branches are
; mutually exclusive by node kind: `(pointer_type (_))` unwraps `*T`
; (and `*T[P]`) to its inner type node; the other two match value
; receivers `T` and `T[P]` directly.
(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (pointer_type (_) @qualifier)
        (type_identifier) @qualifier
        (generic_type) @qualifier
      ]))
  name: (field_identifier) @name) @def

(type_spec
  name: (type_identifier) @name) @def

; ── Call-target references ──────────────────────────────────────────

(call_expression
  function: (identifier) @name) @ref

; Qualified calls dual-emit `pkg.Func` + `Func`. @qualifier is the
; operand of ANY kind — `a.b.Func()` emits `a.b.Func` + `Func`.
(call_expression
  function: (selector_expression
    operand: (_) @qualifier
    field: (field_identifier) @name)) @ref

; ── Identifier-as-VALUE references (mache dead_code fix, bead
; ley-line-open-77c13f follow-up) ────────────────────────────────────

; Composite-literal field value: `{RunE: runServe}`. tree-sitter-go
; wraps both sides in `literal_element`; only a BARE identifier in the
; second (value) position emits — selector expressions and typed
; literals emit through their own subtree patterns.
(keyed_element
  (literal_element)
  (literal_element (identifier) @name)) @ref

; Function-call arguments: each direct-child bare identifier is a
; value-position ref. The enclosing call_expression's own pattern
; handles the call target.
(argument_list
  (identifier) @name) @ref

; ── Imports ─────────────────────────────────────────────────────────

; @alias missing / `.` defaults to the path's last segment (engine
; rule). Blank imports keep their literal `_` alias.
(import_spec
  name: (_)? @alias
  path: (_) @path) @import
