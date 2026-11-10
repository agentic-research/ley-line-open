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

; ── Identifier-as-VALUE references (mache dead_code fix, beads
; ley-line-open-77c13f follow-up and ley-line-open-a9ff82) ───────────
;
; A function named as a value — assigned, passed, returned, placed in
; a composite literal, or taken as a method value (`s.M`) — is a use.
; Each pattern anchors at the value-position CONTAINER (queries match
; downward only, so no pattern can anchor at the identifier and test
; its parent); the container's grammar kind is what tells a value ref
; from a call ref in `node_refs.node_kind` / `kind_id`. A bare
; identifier emits its token; a selector dual-emits `x.M` + `M` with
; qualifier `x`, exactly like a qualified call. The selector form is
; syntactically identical to a field read (`s.Name`), so field reads
; in these positions emit too — tokens with no def stay unbound.
; Left-hand sides of assignments and `:=` are bindings, not uses, and
; do not emit.

; Composite-literal field value: `{RunE: runServe}`.
(keyed_element
  value: (literal_element [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; Unkeyed composite-literal element: `[]func(){a, s.M}`.
(literal_value
  (literal_element [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; Function-call arguments. The enclosing call_expression's own pattern
; handles the call target.
(argument_list [
  (identifier) @name
  (selector_expression
    operand: (_) @qualifier
    field: (field_identifier) @name)
]) @ref

; `var h = helper`
(var_spec
  value: (expression_list [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; `h := helper`
(short_var_declaration
  right: (expression_list [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; `h = helper`
(assignment_statement
  right: (expression_list [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; `return helper`
(return_statement
  (expression_list [
    (identifier) @name
    (selector_expression
      operand: (_) @qualifier
      field: (field_identifier) @name)
  ])) @ref

; ── Imports ─────────────────────────────────────────────────────────

; @alias missing / `.` defaults to the path's last segment (engine
; rule). Blank imports keep their literal `_` alias.
(import_spec
  name: (_)? @alias
  path: (_) @path) @import
