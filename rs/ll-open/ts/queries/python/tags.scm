; Python extraction query — bead ley-line-open-426dfd.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by cli-lib's def_ref_extraction_fidelity_test (Python arm).
;
; Two arms stay imperative in refs.rs::extract_python (leak notes
; there): `Class.method` qualified defs (the qualifier is an ANCESTOR
; of the anchored node; queries match downward only) and
; `import_from_statement` (path is a `{module}.{name}` join; the
; engine's import vocabulary has no join).

; ── Definitions ─────────────────────────────────────────────────────

(function_definition
  name: (identifier) @name) @def

(class_definition
  name: (identifier) @name) @def

; ── Call-target references ──────────────────────────────────────────

; Attribute calls dual-emit `obj.method` + `method`: @qualifier is the
; object of ANY kind, so `a.b.method()` emits `a.b.method` + `method`.
(call
  function: (identifier) @name) @ref

(call
  function: (attribute
    object: (_) @qualifier
    attribute: (identifier) @name)) @ref

; ── Identifier-as-VALUE references (bead ley-line-open-a9ff82) ──────
;
; A function named as a value — `h = helper`, `register(helper)`,
; `Thread(target=worker)`, `{"k": f}`, `[f]`, `return f`, a bare
; `@decorator`, or a bound method `self.meth` — is a use mache's
; dead_code needs. Each pattern anchors at the value-position CONTAINER
; (queries match downward only); the container's grammar kind tells a
; value ref from a call ref in `node_refs.node_kind` / `kind_id`. An
; attribute dual-emits `obj.attr` + `attr` with qualifier `obj`, like
; an attribute call; attribute READS in these positions emit too
; (syntactically identical to a bound method), and stay unbound when
; no def carries the token. Assignment targets do not emit.

(argument_list [
  (identifier) @name
  (attribute object: (_) @qualifier attribute: (identifier) @name)
]) @ref

(keyword_argument
  value: [
    (identifier) @name
    (attribute object: (_) @qualifier attribute: (identifier) @name)
  ]) @ref

(assignment
  right: [
    (identifier) @name
    (attribute object: (_) @qualifier attribute: (identifier) @name)
  ]) @ref

(return_statement [
  (identifier) @name
  (attribute object: (_) @qualifier attribute: (identifier) @name)
]) @ref

(pair
  value: [
    (identifier) @name
    (attribute object: (_) @qualifier attribute: (identifier) @name)
  ]) @ref

(list [
  (identifier) @name
  (attribute object: (_) @qualifier attribute: (identifier) @name)
]) @ref

; `@decorator` / `@pkg.decorator` without a call. `@decorator(...)` is
; a call and emits through the call patterns above.
(decorator [
  (identifier) @name
  (attribute object: (_) @qualifier attribute: (identifier) @name)
]) @ref

; ── Imports ─────────────────────────────────────────────────────────

; `import a.b.c` — the alias is the LAST dotted segment. The engine's
; alias default splits on `/`, which never fires for Python paths, so
; the alias is captured explicitly (`.` anchors the capture to the
; final identifier).
(import_statement
  name: (dotted_name
    (identifier) @alias .) @path) @import

; `import a.b as c` — explicit alias.
(import_statement
  name: (aliased_import
    name: (dotted_name) @path
    alias: (identifier) @alias)) @import
